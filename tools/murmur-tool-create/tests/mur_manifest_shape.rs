//! What a scaffolded manifest is worth, judged by the ecosystem that consumes it rather than by
//! this crate's own string matching.
//!
//! `wasm_component.rs` proves the component writes the bytes the scaffolder intends to write, and
//! the unit tests in `src/lib.rs` prove those bytes contain the keys the scaffolder intends to
//! emit. Neither can see whether `mur` agrees. That gap is exactly how the shipped generator came
//! to emit `runtime: native` with no `implementation:` and no `requires_files:` for years: every
//! assertion compared the generator's output against the generator's own idea of correct output,
//! so the manifest was internally consistent and externally wrong — it published as wasm, and it
//! packed into an archive holding nothing but itself.
//!
//! So each test here scaffolds unedited, hands the directory to a real `mur build`, publishes the
//! result to a `LocalRegistry` rooted at a scratch `HOME`, and reads back the classification the
//! registry recorded plus the entries the archive actually holds.
//!
//! `mur` is built in the murmur repository, never from here — it comes from `MUR_BIN` or from
//! `PATH`, and a run that can find it in neither fails rather than skipping, so a green run of
//! this file always means these tests ran.
//!
//! They are `#[ignore]`d: they need a `mur` the default `cargo test --workspace` has no reason to
//! have. Run them with
//! `MUR_BIN=/path/to/mur cargo test -p murmur-tool-create --test mur_manifest_shape -- --ignored`.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

use murmur_tool_create::logic::scaffold_tool_in;
use serde_json::Value;
use tempfile::TempDir;

/// The version every scaffolded manifest declares, and so the version every artifact built here
/// carries through `mur build` and into the registry.
const SCAFFOLDED_VERSION: &str = "0.1.0";

// ── the runtime under test ───────────────────────────────────────────────────

/// The `mur` binary these tests drive: `MUR_BIN` when set, otherwise the first executable `mur`
/// on `PATH`.
///
/// Panics when neither resolves. A skip would be worse than useless here: this file exists to
/// prove a join between two repositories, and a suite that reports success having run nothing
/// says the join holds when nothing checked it.
fn mur_binary() -> PathBuf {
    if let Some(explicit) = std::env::var_os("MUR_BIN") {
        let path = PathBuf::from(explicit);
        assert!(
            is_executable(&path),
            "MUR_BIN names {}, which is not an executable file",
            path.display()
        );
        return path;
    }

    let path_var = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path_var)
        .map(|dir| dir.join("mur"))
        .find(|candidate| is_executable(candidate))
        .unwrap_or_else(|| {
            panic!(
                "no `mur` binary: set MUR_BIN to one, or put it on PATH. These tests drive the \
                 runtime as a subprocess, so a missing binary is a failure rather than a skip — \
                 a suite that reports success having launched nothing proves nothing. Build one \
                 with `cargo build -p murmur-cli` in a murmur checkout."
            )
        })
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// A `mur` invocation with the scratch `HOME` in place.
///
/// `NEXUS_API_KEY` is removed so a developer's own key cannot turn a local registry publish into
/// a remote one.
fn mur(home: &TempDir) -> Command {
    let mut command = Command::new(mur_binary());
    command.env("HOME", home.path()).env_remove("NEXUS_API_KEY");
    command
}

/// Run a `mur` invocation to completion and return its stdout, panicking with both streams when
/// it fails.
fn run_to_success(mut command: Command, what: &str) -> String {
    let output = command
        .output()
        .unwrap_or_else(|err| panic!("spawning `mur` for {what}: {err}"));
    assert!(
        output.status.success(),
        "{what} failed ({})\n--- stdout ---\n{}\n--- stderr ---\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

// ── the round trip ──────────────────────────────────────────────────────────

/// What `mur build` and `mur publish` made of one unedited scaffold.
struct Published {
    /// The `meta` object inside the `.meta.json` the local registry wrote — `LocalRegistry`
    /// writes the `ArtifactMeta` under that one key rather than at the document root.
    meta: Value,
    /// Every entry name in the built `.mur.zip`.
    entries: Vec<String>,
}

/// Scaffold `name` for `runtime`, build it, publish it locally, and read back both what the
/// registry recorded and what the archive holds.
///
/// The scaffold is used exactly as generated, save for `extra_payload`: `mur build` requires every
/// declared `requires_files:` entry to exist on disk, and a wasm payload is produced by a cargo
/// build the scaffolder does not run. A placeholder byte file satisfies that existence check —
/// the payload-shape rule selects the root wasm by name, and what is under test here is the
/// manifest's classification and the archive's contents, not the payload's validity. The native
/// arm's `bin/<name>` is written by the scaffolder itself, so it passes `extra_payload: None`.
fn scaffold_build_publish(name: &str, runtime: &str, extra_payload: Option<&str>) -> Published {
    let home = TempDir::new().expect("scratch HOME");
    let workspace = TempDir::new().expect("scratch workspace");

    scaffold_tool_in(workspace.path(), name, runtime).expect("scaffold");
    let tool_dir = workspace.path().join("tools").join(name);

    if let Some(payload) = extra_payload {
        fs::write(tool_dir.join(payload), b"\0asm").expect("write placeholder payload");
    }

    // `mur build` defaults its source to the process CWD, and `mur publish` resolves workspace
    // config the same way. Both run from a scratch directory holding no `murmur.yaml`, so nothing
    // from this repository can stand in for the scaffold under test.
    let scratch = TempDir::new().expect("scratch CWD");
    let zip = scratch
        .path()
        .join(format!("{name}-{SCAFFOLDED_VERSION}.mur.zip"));

    let mut build = mur(&home);
    build
        .current_dir(scratch.path())
        .arg("build")
        .arg(&tool_dir)
        .arg("-o")
        .arg(&zip);
    run_to_success(build, &format!("`mur build` of the {runtime} scaffold"));
    assert!(zip.is_file(), "mur build produced no {}", zip.display());

    let mut publish = mur(&home);
    publish
        .current_dir(scratch.path())
        .arg("publish")
        .arg(&zip)
        .arg("--registry")
        .arg("local");
    run_to_success(publish, &format!("`mur publish` of the {runtime} scaffold"));

    let meta_path = home
        .path()
        .join(".murmur/artifacts")
        .join(name)
        .join(SCAFFOLDED_VERSION)
        .join(format!("{name}-{SCAFFOLDED_VERSION}.meta.json"));
    let document: Value = serde_json::from_str(
        &fs::read_to_string(&meta_path)
            .unwrap_or_else(|err| panic!("reading {}: {err}", meta_path.display())),
    )
    .expect("meta.json");
    let meta = document
        .get("meta")
        .unwrap_or_else(|| panic!("{} has no `meta` object: {document}", meta_path.display()))
        .clone();

    Published { meta, entries: zip_entries(&zip) }
}

/// The entry names of a `.mur.zip`, read with `unzip -Z1`.
///
/// Shelling out rather than adding a `zip` dev-dependency: this file already drives an external
/// binary as its whole subject, and `unzip` reports the archive as any consumer would read it
/// rather than as a Rust crate reconstructs it.
fn zip_entries(zip: &Path) -> Vec<String> {
    let output = Command::new("unzip")
        .arg("-Z1")
        .arg(zip)
        .output()
        .unwrap_or_else(|err| panic!("spawning `unzip -Z1` for {}: {err}", zip.display()));
    assert!(
        output.status.success(),
        "`unzip -Z1 {}` failed ({})\n{}",
        zip.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Assert the registry classified the artifact as expected and the archive carries both the
/// manifest and the declared payload.
fn assert_published(published: &Published, runtime: &str, artifact_runtime: &str, payload: &str) {
    assert_eq!(
        published.meta["runtime"], runtime,
        "registry runtime; meta: {}",
        published.meta
    );
    assert_eq!(
        published.meta["artifact_runtime"], artifact_runtime,
        "manifest runtime verbatim; meta: {}",
        published.meta
    );
    assert!(
        published.entries.iter().any(|e| e == "murmur.yaml"),
        "archive is missing murmur.yaml; entries: {:?}",
        published.entries
    );
    // The pre-fix scaffolder declared no `requires_files:`, so `plan_packed_entries` packed the
    // manifest and nothing else — an archive with no artifact in it.
    assert!(
        published.entries.iter().any(|e| e == payload),
        "archive is missing the declared payload {payload}; entries: {:?}",
        published.entries
    );
}

// ── the three arms ──────────────────────────────────────────────────────────

#[test]
#[ignore = "drives the `mur` binary as a subprocess; see the module docs"]
fn wasm_scaffold_publishes_as_a_wasm_tool() {
    let published = scaffold_build_publish("shape-probe-wasm", "wasm", Some("shape_probe_wasm.wasm"));
    assert_published(&published, "wasm", "tool", "shape_probe_wasm.wasm");
}

#[test]
#[ignore = "drives the `mur` binary as a subprocess; see the module docs"]
fn native_scaffold_publishes_as_a_native_tool() {
    // The classification the shipped scaffolder got wrong: with no `implementation:` to read,
    // `registry_runtime()` fell through to `Wasm` and the artifact published as a wasm tool.
    let published = scaffold_build_publish("shape-probe-native", "native", None);
    assert_published(&published, "native", "tool", "bin/shape-probe-native");
}

#[test]
#[ignore = "drives the `mur` binary as a subprocess; see the module docs"]
fn hook_scaffold_publishes_as_a_wasm_hook() {
    let published = scaffold_build_publish("shape-probe-hook", "hook", Some("shape_probe_hook.wasm"));
    assert_published(&published, "wasm", "hook", "shape_probe_hook.wasm");
}
