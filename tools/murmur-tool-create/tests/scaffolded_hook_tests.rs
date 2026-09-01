//! Whether a freshly scaffolded hook is testable, judged by running its tests rather than
//! by matching strings in its source.
//!
//! The unit tests in `src/lib.rs` assert the generated source has a `pub mod logic`, one
//! `#[cfg(target_arch = "wasm32")]` gate after it, and a `#[test]`. None of that proves the
//! crate compiles for the host, and none of it proves the test inside it runs. That gap is
//! exactly the defect this scaffold arm shipped with: the previous generated crate was
//! valid Rust that `cargo test` would happily report green on, having compiled nothing and
//! run nothing, because every item sat behind the wasm gate.
//!
//! So this test scaffolds a hook into a `TempDir` — outside any cargo workspace, so the
//! standalone generated `Cargo.toml` resolves on its own — runs `cargo test` inside it with
//! a scratch `CARGO_TARGET_DIR`, and reads the child's own result line. A child reporting
//! `0 passed` fails here; that is the precise condition the split exists to make
//! impossible.
//!
//! It is `#[ignore]`d: it needs a cargo registry able to resolve `wit-bindgen 0.59` and
//! `tempfile 3`, which the default `cargo test --workspace` has no reason to guarantee. It
//! fails rather than skipping when a prerequisite is missing, so a green run of this file
//! always means the child cargo really ran. Run it with
//! `cargo test -p murmur-tool-create --test scaffolded_hook_tests -- --ignored`.

use std::{env, process::Command};

use murmur_tool_create::logic::scaffold_tool_in;
use tempfile::TempDir;

/// The unit-test result line the child must print. Matched in full rather than on
/// `"1 passed"` alone, because `cargo test` also runs a doc-test binary whose own line
/// reads `0 passed` for a crate with no doc examples.
const EXPECTED_RESULT: &str = "test result: ok. 1 passed";

#[test]
#[ignore = "runs a child `cargo test`, which needs a registry with wit-bindgen 0.59 and tempfile 3"]
fn a_scaffolded_hook_passes_its_own_cargo_test() {
    let tmp = TempDir::new().expect("failed to create a scratch directory");
    scaffold_tool_in(tmp.path(), "born-testable", "hook").expect("scaffolding a hook failed");

    let crate_dir = tmp.path().join("tools").join("born-testable");
    // Kept inside the same scratch directory so the run leaves nothing behind, and so it
    // cannot share this workspace's target dir with a different set of features.
    let target_dir = tmp.path().join("cargo-target");

    // `CARGO` is the cargo that launched this test, which is the one the repo's
    // rust-toolchain.toml pinned. Falling back to a bare `cargo` keeps the test runnable
    // outside a cargo-driven invocation.
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());

    let output = Command::new(&cargo)
        .arg("test")
        .current_dir(&crate_dir)
        .env("CARGO_TARGET_DIR", &target_dir)
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "failed to run `{cargo} test` in {}: {e}",
                crate_dir.display()
            )
        });

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "`cargo test` inside the scaffolded hook failed ({}).\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        output.status
    );

    // The point of the whole slice: the generated crate has host-target code, and a test
    // over it that actually executed.
    assert!(
        stdout.contains(EXPECTED_RESULT),
        "the scaffolded hook's own test run should report {EXPECTED_RESULT:?}; a run reporting \
         `0 passed` means the crate compiled to nothing for the host target.\n--- stdout ---\n\
         {stdout}\n--- stderr ---\n{stderr}"
    );
}
