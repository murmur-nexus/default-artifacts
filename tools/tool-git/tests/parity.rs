//! Same answers as the published 0.6.0 build.
//!
//! `tests/fixtures/native-0.6.0-results.json` is a frozen recording of [`common::sequence`] run
//! over a fresh [`common::fixture`] folder by tool-git 0.6.0: what the model saw for each call and
//! the argv of every git command it ran, normalised. Its provenance is in
//! `tests/fixtures/README.md`. `native_0_6_0_results` runs the same sequence through the
//! component (and `native_0_6_0_results_core` through the host-side core) and compares: `status`
//! and `summary` exactly, `data` as a JSON value, and the argv of every git command.

#![cfg(unix)]

mod common;

use std::{
    fs,
    path::{Path, PathBuf},
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
                    "{} {key}:\n  fixture:   {}\n  {backend:?}: {}",
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
