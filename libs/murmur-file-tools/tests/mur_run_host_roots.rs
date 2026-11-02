//! Host roots — the host folders murmur hands an unscoped WASM tool besides the work folder —
//! proved under a real `mur run`.
//!
//! `tests/wasm_components.rs` proves the resolution against preopens the test makes itself. Only
//! a launch shows that murmur hands the roots as the tools expect: the whole host, read and
//! write, at `murmur-host` under `mur run --unconfined`, and each `host_paths` entry of
//! `~/.murmur/config.yaml` at `murmur-host<path>`, with its access, in a contained session. It
//! also shows that `capabilities.filesystem.read_only` still refuses, before dispatch, a write
//! naming the work folder's absolute host path.
//!
//! The harness is `tests/common`: the real `mur` binary as a subprocess with
//! `HOME` pointed at a scratch directory, against a loopback fake Chat Completions provider that
//! scripts the model's tool calls and records every request body. The four tools and
//! `driver-openai` are built out of this workspace on every run; `mur` comes from `MUR_BIN` or
//! `PATH`, and a run that finds neither fails rather than skipping. It needs a `mur` at murmur
//! `e86e0b42` or later, the first that hands a WASM tool its host roots.
//!
//! It is `#[ignore]`d because the default `cargo test --workspace` has no `mur` to launch.
//! `MUR_BIN=$(command -v mur) cargo test -p murmur-file-tools --test mur_run_host_roots --
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

    /// The work folder, created, by its canonical absolute path.
    fn workdir(&self) -> PathBuf {
        let workdir = self.project.path().join("work");
        fs::create_dir_all(&workdir).unwrap();
        fs::canonicalize(workdir).unwrap()
    }

    /// The scratch `HOME`, by its canonical absolute path.
    fn home(&self) -> PathBuf {
        fs::canonicalize(self.home.path()).unwrap()
    }

    /// The capsule manifest, with `read_only` entries when given. `MODEL_ROUTES` sends
    /// `chat-latest` to the Chat Completions surface.
    fn write_manifest(&self, endpoint: &str, read_only: &[&str]) {
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
        let capabilities = if read_only.is_empty() {
            String::new()
        } else {
            let entries: String = read_only
                .iter()
                .map(|entry| format!("      - {entry}\n"))
                .collect();
            format!("capabilities:\n  filesystem:\n    read_only:\n{entries}")
        };
        fs::write(
            self.manifest(),
            format!(
                "name: host-roots-proof\nversion: 0.1.0\nartifacts:\n\
                 \x20 - name: local/driver-openai\n    version: {}\n    runtime: driver\n    \
                 gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
                 {tools}\
                 {capabilities}\
                 inference:\n  transport: http\n  model: chat-latest\n  max_tokens: 4000\n  \
                 driver:\n    artifact: local/driver-openai\n",
                self.driver_version
            ),
        )
        .unwrap();
    }

    /// Add `host_paths:` to the scratch `~/.murmur/config.yaml`, keeping what staging wrote.
    fn lend_host_path(&self, dir: &Path, access: &str) {
        let config = self.home.path().join(".murmur/config.yaml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let existing = fs::read_to_string(&config).unwrap_or_default();
        assert!(
            !existing.contains("host_paths"),
            "the scratch config already lends host paths:\n{existing}"
        );
        fs::write(
            &config,
            format!(
                "{existing}{}host_paths:\n  - path: {}\n    access: {access}\n",
                if existing.is_empty() || existing.ends_with('\n') {
                    ""
                } else {
                    "\n"
                },
                dir.display()
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
    staging.write_manifest("http://127.0.0.1:1/v1", &[]);

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

/// Run one task to completion, with `extra` flags before `--task`, and return its output,
/// whatever its exit status.
fn run_session(staging: &Staging, extra: &[&str]) -> Output {
    mur(&staging.home)
        .current_dir(staging.project.path())
        .arg("run")
        .args(extra)
        .args([
            "--manifest",
            staging.manifest().to_str().unwrap(),
            "--task",
            "use the host folders",
            "--workdir",
            staging.workdir().to_str().unwrap(),
        ])
        .output()
        .expect("spawning `mur run`")
}

// ── the scenarios ────────────────────────────────────────────────────────────

/// Runs `script` in `staging` with `extra` flags and returns each call's tool result, by call
/// number from 1, after asserting the session succeeded.
fn run_script(
    staging: &Staging,
    read_only: &[&str],
    extra: &[&str],
    script: Vec<(Tool, Value)>,
) -> Vec<String> {
    let calls = script.len();
    let provider = Provider::start(script);
    staging.write_manifest(&provider.endpoint, read_only);
    let output = run_session(staging, extra);
    let text = combined(&output);
    println!("{text}");
    assert!(output.status.success(), "the session must succeed:\n{text}");

    let requests = provider.requests();
    let results = tool_results(
        requests
            .last()
            .expect("the provider was sent at least one request"),
    );
    for (id, content) in &results {
        println!("{id}: {content}");
    }
    (1..=calls)
        .map(|n| {
            let id = format!("call_{n}");
            results
                .iter()
                .find(|(call, _)| *call == id)
                .map(|(_, content)| content.clone())
                .unwrap_or_else(|| panic!("no tool result for {id} in {results:?}"))
        })
        .collect()
}

/// A scratch directory outside every temp folder and the work folder, for a `host_paths`
/// entry: `mur run` refuses one under `/tmp`, where `tempfile` may put its directories.
fn lendable_dir(tag: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".test-scratch")
        .join(format!("mur_run_{tag}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::canonicalize(dir).unwrap()
}

/// Under `--unconfined` the tools hold the whole host: a read of a file in the scratch `HOME`
/// and a write outside the work folder, both by absolute path, work, and `read_only` still
/// refuses a write naming the work folder's absolute host path before dispatch.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn unconfined_tools_reach_the_whole_host_and_read_only_still_holds() {
    let staging = stage();
    let home = staging.home();
    let workdir = staging.workdir();
    fs::create_dir_all(workdir.join("protected")).unwrap();
    let secret = home.join("host-notes.txt");
    fs::write(&secret, "token-from-the-host-home\n").unwrap();
    let outside = staging.project.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let written = fs::canonicalize(&outside).unwrap().join("made/by-tool.txt");
    let protected = workdir.join("protected/x.txt");

    let results = run_script(
        &staging,
        &["protected"],
        &["--unconfined"],
        vec![
            (Tool::Read, json!({ "path": secret })),
            (
                Tool::Write,
                json!({ "path": written, "content": "written on the host\n" }),
            ),
            (Tool::Write, json!({ "path": protected, "content": "x" })),
        ],
    );

    assert!(
        results[0].contains("token-from-the-host-home"),
        "the read in HOME returns the file: {}",
        results[0]
    );
    assert!(
        !results[1].contains("Refused") && !results[1].contains("absolute_path"),
        "the write outside the work folder runs: {}",
        results[1]
    );
    assert_eq!(
        fs::read_to_string(&written).unwrap(),
        "written on the host\n",
        "the write by absolute path created the host file and its folder"
    );
    assert!(
        results[2].contains("Refused by the capsule manifest")
            && results[2].contains("destination 'path' declared by the tool's input schema"),
        "the write naming <workdir>/protected absolutely is refused before dispatch: {}",
        results[2]
    );
    assert!(!protected.exists(), "the refused write creates nothing");
}

/// In a contained session with one `read` host path, the tools hold that folder alone,
/// read-only: a read inside it works, a write inside it is refused naming it read-only, and a
/// read of another absolute path is refused listing it.
#[test]
#[ignore = "launches the real mur binary; run with --ignored"]
fn contained_tools_reach_a_read_host_path_only() {
    let staging = stage();
    let lent = lendable_dir("host_roots_lent");
    fs::write(lent.join("lent.txt"), "lent-folder-content\n").unwrap();
    staging.lend_host_path(&lent, "read");
    let elsewhere = staging.home().join("elsewhere.txt");
    fs::write(&elsewhere, "not lent\n").unwrap();
    let blocked = lent.join("new.txt");

    let results = run_script(
        &staging,
        &[],
        &[],
        vec![
            (Tool::Read, json!({ "path": lent.join("lent.txt") })),
            (Tool::Write, json!({ "path": blocked, "content": "x" })),
            (Tool::Read, json!({ "path": elsewhere })),
        ],
    );

    let lent_path = lent.display().to_string();
    assert!(
        results[0].contains("lent-folder-content"),
        "the read in the host path returns the file: {}",
        results[0]
    );
    assert!(
        results[1].contains(&format!(
            "'path' '{}' is in {lent_path}, which this session opens read-only",
            blocked.display()
        )),
        "the write in the read host path is refused naming it: {}",
        results[1]
    );
    assert!(!blocked.exists(), "the refused write creates nothing");
    assert!(
        results[2].contains(&format!(
            "'path' is outside every host folder tool-read may open: {lent_path} (read); got '{}'",
            elsewhere.display()
        )),
        "the read elsewhere is refused listing the host path: {}",
        results[2]
    );
    assert!(!results[2].contains("not lent"), "{}", results[2]);
    let _ = fs::remove_dir_all(&lent);
}
