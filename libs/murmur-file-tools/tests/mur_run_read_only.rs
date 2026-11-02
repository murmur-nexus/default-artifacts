//! `capabilities.filesystem.read_only` against the four file tools, proved under a real `mur run`.
//!
//! `tests/manifests.rs` holds the manifests to two destination locations, `tool-write`'s and
//! `tool-edit`'s `path`, and to `murmur-destinations: []` on `tool-read` and `tool-find`. Only a
//! launch shows that murmur lowers them as declared: a read and a find under a `read_only` entry
//! run, an edit or a write of a path under it is refused before dispatch naming the declared
//! location, and an edit and a write outside it apply.
//!
//! The test drives the real `mur` binary as a subprocess with `HOME` pointed at a scratch
//! directory, against a loopback fake Chat Completions provider that scripts the model's tool
//! calls and records every request body. The four tools and `driver-openai` are built out of
//! this workspace on every run; `mur` comes from `MUR_BIN` or `PATH`, and a run that finds
//! neither fails rather than skipping. The last request's `tools` array is written to the test's
//! temp dir and measured, which is the request's tool array token count on the loopback.
//!
//! It is `#[ignore]`d because the default `cargo test --workspace` has no `mur` to launch.
//! `MUR_BIN=$(command -v mur) cargo test -p murmur-file-tools --test mur_run_read_only --
//! --ignored --nocapture` runs it.

#![cfg(unix)]

mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    process::Output,
};

use common::{combined, mur, pack, run_to_success, tool_results, Provider};
use murmur_file_tools::Tool;
use serde_json::{json, Value};
use tempfile::TempDir;

// ── staging ──────────────────────────────────────────────────────────────────

struct Staging {
    home: TempDir,
    project: TempDir,
    driver_version: String,
    /// `(tool, version)` for each of the four file tools.
    tool_versions: Vec<(Tool, String)>,
    _artifacts: TempDir,
}

impl Staging {
    fn manifest(&self) -> PathBuf {
        self.project.path().join("murmur.yaml")
    }

    fn workdir(&self) -> PathBuf {
        self.project.path().join("work")
    }

    /// The capsule manifest: `src` is read-only. `MODEL_ROUTES` sends `chat-latest` to the
    /// Chat Completions surface.
    fn write_manifest(&self, endpoint: &str) {
        let tools: String = self
            .tool_versions
            .iter()
            .map(|(tool, version)| {
                format!(
                    "  - name: local/{}\n    version: {version}\n    runtime: tool\n",
                    tool.name()
                )
            })
            .collect();
        fs::write(
            self.manifest(),
            format!(
                "name: read-only-proof\nversion: 0.1.0\nartifacts:\n\
                 \x20 - name: local/driver-openai\n    version: {}\n    runtime: driver\n    \
                 gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
                 {tools}\
                 capabilities:\n  filesystem:\n    read_only:\n      - src\n\
                 inference:\n  transport: http\n  model: chat-latest\n  max_tokens: 4000\n  \
                 driver:\n    artifact: local/driver-openai\n",
                self.driver_version
            ),
        )
        .unwrap();
    }
}

fn stage() -> Staging {
    let artifacts = tempfile::tempdir().unwrap();
    let (driver_version, driver_zip) = pack("drivers/driver-openai", artifacts.path());
    let mut zips = vec![(driver_zip, "driver-openai")];
    let mut tool_versions = Vec::new();
    for tool in Tool::ALL {
        let (version, zip) = pack(&format!("tools/{}", tool.name()), artifacts.path());
        tool_versions.push((tool, version));
        zips.push((zip, tool.name()));
    }
    let staging = Staging {
        home: tempfile::tempdir().unwrap(),
        project: tempfile::tempdir().unwrap(),
        driver_version,
        tool_versions,
        _artifacts: artifacts,
    };
    staging.write_manifest("http://127.0.0.1:1/v1");

    for (zip, what) in &zips {
        let mut publish = mur(&staging.home);
        publish.arg("publish").arg(zip);
        run_to_success(publish, &format!("publishing {what}"));

        let mut install = mur(&staging.home);
        install
            .current_dir(staging.project.path())
            .arg("install")
            .arg(zip);
        run_to_success(install, &format!("installing {what}"));
    }
    staging
}

/// Run one task to completion and return its output, whatever its exit status.
fn run_session(staging: &Staging) -> Output {
    mur(&staging.home)
        .current_dir(staging.project.path())
        .args([
            "run",
            "--manifest",
            staging.manifest().to_str().unwrap(),
            "--task",
            "apply the changes in src/spec.txt to notes.txt",
            "--workdir",
            staging.workdir().to_str().unwrap(),
        ])
        .output()
        .expect("spawning `mur run`")
}

// ── the fake provider ────────────────────────────────────────────────────────

/// The tool calls the fake model makes, one per turn, in order, each as `(tool, arguments)`; the
/// turn after the last one answers with text.
fn scripted_calls() -> Vec<(Tool, Value)> {
    vec![
        (Tool::Read, json!({ "path": "src/spec.txt" })),
        (Tool::Find, json!({ "pattern": "colour", "dir": "src" })),
        (
            Tool::Edit,
            json!({ "path": "src/spec.txt", "edits": [{ "old": "alpha", "new": "beta" }] }),
        ),
        (
            Tool::Write,
            json!({ "path": "src/new.txt", "content": "x" }),
        ),
        (
            Tool::Edit,
            json!({ "path": "notes.txt", "edits": [
                { "old": "colour", "new": "color" },
                { "old": "draft", "new": "final" },
            ] }),
        ),
        (
            Tool::Write,
            json!({ "path": "out/new.txt", "content": "written\n" }),
        ),
    ]
}

// ── the scenario ─────────────────────────────────────────────────────────────

const SPEC: &str = "alpha spec: replace colour with color, draft with final\n";
const NOTES: &str = "the colour of the draft\n";

/// The `(chars, cl100k tokens)` of `text`.
fn measure(bpe: &tiktoken_rs::CoreBPE, text: &str) -> (usize, usize) {
    (
        text.chars().count(),
        bpe.encode_with_special_tokens(text).len(),
    )
}

/// The request's `tools` array, written to `dir/tools.json`, then each file tool entry measured
/// as murmur counts a definition (trimmed description plus compact parameters), and as the whole
/// serialized entry, wrapper keys included.
fn report_tool_array(request: &str, dir: &Path) {
    let body: Value = serde_json::from_str(request).expect("the request body is JSON");
    let tools = body["tools"].as_array().expect("the request carries tools");
    let path = dir.join("tools.json");
    fs::write(&path, serde_json::to_string(&body["tools"]).unwrap()).unwrap();
    println!("tools array written to {}", path.display());

    let bpe = tiktoken_rs::cl100k_base().expect("the cl100k_base encoding loads");
    let (mut def_total, mut entry_total) = ((0, 0), (0, 0));
    for tool in Tool::ALL {
        let entry = tools
            .iter()
            .find(|entry| entry["function"]["name"] == tool.name())
            .unwrap_or_else(|| panic!("no {} entry in the request's tools", tool.name()));
        let function = &entry["function"];
        let description = function["description"].as_str().unwrap_or("").trim();
        let parameters = function["parameters"].to_string();
        let (d, p) = (measure(&bpe, description), measure(&bpe, &parameters));
        let whole = measure(&bpe, &entry.to_string());
        println!(
            "{:<11} definition {:>4} chars {:>3} tokens | entry {:>4} chars {:>3} tokens",
            tool.name(),
            d.0 + p.0,
            d.1 + p.1,
            whole.0,
            whole.1
        );
        def_total = (def_total.0 + d.0 + p.0, def_total.1 + d.1 + p.1);
        entry_total = (entry_total.0 + whole.0, entry_total.1 + whole.1);
    }
    println!(
        "four tools  definition {:>4} chars {:>3} tokens | entry {:>4} chars {:>3} tokens",
        def_total.0, def_total.1, entry_total.0, entry_total.1
    );
}

/// A read and a find under `src` run; an edit and a write under it are refused naming the
/// declared `path`; an edit and a write outside it apply.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn read_only_refuses_edit_and_write_by_declared_location_and_allows_the_reads() {
    let staging = stage();
    let workdir = staging.workdir();
    fs::create_dir_all(workdir.join("src")).unwrap();
    fs::write(workdir.join("src/spec.txt"), SPEC).unwrap();
    fs::write(workdir.join("notes.txt"), NOTES).unwrap();

    let provider = Provider::start(scripted_calls());
    staging.write_manifest(&provider.endpoint);

    let output = run_session(&staging);
    let text = combined(&output);
    println!("{text}");
    assert!(output.status.success(), "the session must succeed:\n{text}");
    assert!(
        !text.contains("W-SEC-018"),
        "no file tool is flagged as an undeclared writer:\n{text}"
    );

    let requests = provider.requests();
    let last = requests
        .last()
        .expect("the provider was sent at least one request");
    let results = tool_results(last);
    let result = |id: &str| -> &str {
        results
            .iter()
            .find(|(call, _)| call == id)
            .map(|(_, content)| content.as_str())
            .unwrap_or_else(|| panic!("no tool result for {id} in {results:?}"))
    };
    for (id, content) in &results {
        println!("{id}: {content}");
    }
    report_tool_array(last, staging.project.path());

    assert!(
        result("call_1").contains("alpha spec: replace colour with color"),
        "the read under src returns the file: {}",
        result("call_1")
    );
    assert!(
        result("call_2").contains("spec.txt") && !result("call_2").contains("Refused"),
        "the find under src returns its match: {}",
        result("call_2")
    );
    for id in ["call_3", "call_4"] {
        let refusal = result(id);
        assert!(
            refusal.contains("Refused by the capsule manifest")
                && refusal.contains("destination 'path' declared by the tool's input schema"),
            "{id} must be refused naming path: {refusal}"
        );
    }
    for id in ["call_5", "call_6"] {
        assert!(
            !result(id).contains("Refused"),
            "{id} outside src runs: {}",
            result(id)
        );
    }

    assert_eq!(
        fs::read(workdir.join("src/spec.txt")).unwrap(),
        SPEC.as_bytes(),
        "the refused edit leaves src/spec.txt byte-identical"
    );
    assert!(
        !workdir.join("src/new.txt").exists(),
        "the refused write creates nothing"
    );
    assert_eq!(
        fs::read_to_string(workdir.join("notes.txt")).unwrap(),
        "the color of the final\n",
        "the edit outside src applied both pairs"
    );
    assert_eq!(
        fs::read_to_string(workdir.join("out/new.txt")).unwrap(),
        "written\n",
        "the write outside src created its directory and file"
    );
}
