//! Same answers as the native 0.6.0 build.
//!
//! `record_native` (ignored) runs [`common::sequence`] over a fresh [`common::fixture`] folder
//! through the native 0.6.0 binary named by `TOOL_GIT_NATIVE_BIN`, maps each stdout object as
//! murmur's native dispatch did, records the argv of every git the binary ran (through a `git`
//! shim first on `PATH`), normalises both and writes `tests/fixtures/native-0.6.0-results.json`.
//! `native_0_6_0_results` runs the same sequence through the component and compares: `status`
//! and `summary` exactly, `data` as a JSON value, and the argv of every git command. The recipe
//! for the native binary is in `tests/fixtures/README.md`.

#![cfg(unix)]

mod common;

use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use common::{normalise::Normaliser, sequence};
use serde_json::{json, Value};

const FIXTURE: &str = "tests/fixtures/native-0.6.0-results.json";

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE)
}

/// The paths a run's work folder is reached at: as created, and canonical.
fn work_names(work: &Path) -> Vec<String> {
    let mut names = vec![work.display().to_string()];
    let canonical = fs::canonicalize(work).unwrap().display().to_string();
    if !names.contains(&canonical) {
        names.push(canonical);
    }
    names
}

fn parse(data: Option<String>) -> Value {
    match data {
        None => Value::Null,
        Some(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
    }
}

/// One recorded call: what the model saw, and the argv of each git command it ran.
fn entry(step: &str, result: Value, argv: Vec<Vec<String>>) -> Value {
    json!({
        "step": step,
        "status": result["status"],
        "summary": result["summary"],
        "data": result["data"],
        "argv": argv,
    })
}

#[test]
#[ignore = "needs TOOL_GIT_NATIVE_BIN, the native 0.6.0 binary; see tests/fixtures/README.md"]
fn record_native() {
    let bin =
        PathBuf::from(std::env::var_os("TOOL_GIT_NATIVE_BIN").expect("set TOOL_GIT_NATIVE_BIN"));
    let work = common::scratch("parity-native");
    common::fixture::build(&work);
    let home = common::scratch("parity-native-home");

    // A `git` first on PATH that appends its argv (each argument NUL-terminated, then RS) to
    // `argv.log` and runs the real git.
    let real_git = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let shim = common::scratch("parity-native-shim");
    let log = shim.join("argv.log");
    let script = shim.join("git");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\0' \"$a\"; done >> '{log}'\n\
             printf '\\036' >> '{log}'\nexec '{git}' \"$@\"\n",
            log = log.display(),
            git = real_git.trim()
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        shim.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let mut results = Vec::new();
    for step in sequence::steps() {
        let _ = fs::remove_file(&log);
        let mut child = Command::new(&bin)
            .current_dir(&work)
            .env_clear()
            .env("PATH", &path)
            .env("HOME", &home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("native binary starts");
        let envelope = json!({"data": step.data.to_string(), "log_path": null}).to_string();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(envelope.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let stdout: Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("{}: stdout not JSON ({e})", step.name));
        // murmur's native dispatch: status, summary, and data as a string or compact JSON.
        let outcome = tool_git::NativeOutcome::of(&stdout);
        let result = json!({
            "status": match outcome.status {
                tool_git::OutcomeStatus::Passed => "passed",
                tool_git::OutcomeStatus::Failed => "failed",
                tool_git::OutcomeStatus::Error => "error",
            },
            "summary": outcome.summary,
            "data": parse(outcome.data),
        });
        let argv: Vec<Vec<String>> = fs::read(&log)
            .unwrap_or_default()
            .split(|b| *b == 0x1e)
            .filter(|r| !r.is_empty())
            .map(|r| {
                r.split(|b| *b == 0)
                    .filter(|a| !a.is_empty())
                    .map(|a| String::from_utf8_lossy(a).into_owned())
                    .collect()
            })
            .collect();
        results.push(entry(step.name, result, argv));
    }

    let results = Normaliser::new(&work_names(&work)).run(&results);
    let doc = json!({
        "recorded_from": "native tool-git 0.6.0, built from default-artifacts 95e17e1",
        "results": results,
    });
    fs::create_dir_all(fixture_path().parent().unwrap()).unwrap();
    fs::write(
        fixture_path(),
        serde_json::to_string_pretty(&doc).unwrap() + "\n",
    )
    .unwrap();
    eprintln!("wrote {}", fixture_path().display());
}

/// Run the sequence through `backend` over a fresh fixture folder and normalise it.
pub fn run_sequence(backend: common::Backend) -> Vec<Value> {
    let work = common::scratch("parity");
    common::fixture::build(&work);
    let env = common::Env::new(backend, &work);
    let mut results = Vec::new();
    for step in sequence::steps() {
        let before = env.records().len();
        let res = env.call(step.data.clone());
        let records = env.records();
        let mut argv = Vec::new();
        for record in &records[before..] {
            let r = &record.request;
            assert!(record.ran, "{}: a git command did not run", step.name);
            assert_eq!(r.program, "git", "{}", step.name);
            assert!(r.env.is_empty(), "{}: env {:?}", step.name, r.env);
            assert_eq!(r.stdin, None, "{}", step.name);
            assert_eq!(r.timeout_ms, None, "{}", step.name);
            argv.push(r.args.clone());
        }
        assert_eq!(res["truncated"], false, "{}", step.name);
        results.push(entry(step.name, res, argv));
    }
    Normaliser::new(&work_names(&work)).run(&results)
}

fn compare(backend: common::Backend) {
    let fixture: Value =
        serde_json::from_str(&fs::read_to_string(fixture_path()).expect("read the fixture"))
            .unwrap();
    let want = fixture["results"].as_array().unwrap();
    let got = run_sequence(backend);
    assert_eq!(got.len(), want.len(), "sequence length");
    let mut diffs = Vec::new();
    for (g, w) in got.iter().zip(want) {
        for key in ["step", "status", "summary", "data", "argv"] {
            if g[key] != w[key] {
                diffs.push(format!(
                    "{} {key}:\n  native:    {}\n  {backend:?}: {}",
                    w["step"], w[key], g[key]
                ));
            }
        }
    }
    assert!(diffs.is_empty(), "differences:\n{}", diffs.join("\n"));
}

#[test]
fn native_0_6_0_results() {
    compare(common::Backend::Component);
}

#[test]
fn native_0_6_0_results_core() {
    compare(common::Backend::Core);
}

/// The values listed under `name:` → `enum:` in the manifest's input schema, and each
/// operation's `subcommands:` from `allowed_flags`.
fn manifest_lists() -> (Vec<String>, Vec<(String, Vec<String>)>) {
    let text =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("murmur.yaml")).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let start = lines
        .iter()
        .position(|l| *l == "    operation:")
        .expect("operation property");
    let enum_at = start
        + lines[start..]
            .iter()
            .position(|l| l.trim() == "enum:")
            .unwrap();
    let ops = lines[enum_at + 1..]
        .iter()
        .take_while(|l| l.trim_start().starts_with("- "))
        .map(|l| l.trim_start()[2..].trim().to_string())
        .collect();
    let mut subs = Vec::new();
    let mut current = String::new();
    let flags_at = lines.iter().position(|l| *l == "allowed_flags:").unwrap();
    for l in &lines[flags_at + 1..] {
        if l.starts_with("  ") && !l.starts_with("    ") && l.trim_end().ends_with(':') {
            current = l.trim().trim_end_matches(':').to_string();
        } else if let Some(list) = l.trim().strip_prefix("subcommands: [") {
            let names = list
                .trim_end_matches(']')
                .split(',')
                .map(|s| s.trim().trim_matches('"').to_string())
                .collect();
            subs.push((current.clone(), names));
        } else if !l.starts_with(' ') && !l.is_empty() {
            break;
        }
    }
    (ops, subs)
}

#[test]
fn the_sequence_covers_every_operation_and_subcommand() {
    let (ops, subs) = manifest_lists();
    assert_eq!(ops.len(), 23, "{ops:?}");
    let steps = sequence::steps();
    for op in &ops {
        assert!(
            steps.iter().any(|s| s.data["operation"] == op.as_str()),
            "the sequence never calls {op}"
        );
    }
    assert_eq!(subs.len(), 5, "{subs:?}");
    for (op, names) in subs {
        for name in names {
            assert!(
                steps
                    .iter()
                    .any(|s| s.data["operation"] == op.as_str()
                        && s.data["subcommand"] == name.as_str()),
                "the sequence never calls {op}/{name}"
            );
        }
    }
}

#[test]
fn the_normaliser_maps_hashes_dates_and_the_work_folder() {
    let a = "0123456789abcdef0123456789abcdef01234567";
    let b = "fedcba9876543210fedcba9876543210fedcba98";
    let results = vec![
        json!({"summary": format!("pushed {}", &b[..7]), "data": {"hash": a}}),
        json!({"data": format!("{b} at /w/x/repo on 2026-01-01T00:00:01+00:00, 1a2b3c4..added")}),
    ];
    let out = Normaliser::new(&["/w/x".into()]).run(&results);
    assert_eq!(out[0]["summary"], "pushed <C2:7>");
    assert_eq!(out[0]["data"]["hash"], "<C1>");
    assert_eq!(out[1]["data"], "<C2> at <WORK>/repo on <DATE>, <H1>..added");
}
