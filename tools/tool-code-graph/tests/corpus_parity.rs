//! Real-world parity and call time against the native 0.6.0 binary: a one-off evidence step,
//! `#[ignore]`d because it needs that binary and a corpus tree.
//!
//! ```text
//! CODE_GRAPH_NATIVE_BIN=<native 0.6.0 tool-code-graph> \
//! CODE_GRAPH_CORPUS=<scratch>/default-artifacts \
//! CODE_GRAPH_ROUNDS=7 \
//! CODE_GRAPH_WASM_B=<another build of the component, optional> \
//!   cargo test --release -p tool-code-graph --test corpus_parity -- --ignored --nocapture
//! ```
//!
//! The native binary indexes `CODE_GRAPH_CORPUS` in place (absolute `repo_path`, cwd the tree).
//! The component indexes a copy at `<parent>/wasm/<name>`, with `<parent>/wasm` as its work
//! folder and `repo_path` `<name>`, so the folder name — the package of files outside any
//! `[package]` — is the same in both. The two databases must hold the same rows. Then each
//! operation is timed in interleaved native/component rounds, and min and median are printed.

mod common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{call, call_component, snapshot, Outcome};
use serde_json::{json, Value};

fn native(bin: &Path, repo: &Path, args: &Value) -> Value {
    let mut child = Command::new(bin)
        .current_dir(repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("start the native binary");
    let envelope = json!({ "data": args.to_string(), "log_path": null }).to_string();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(envelope.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    serde_json::from_slice(&out.stdout).expect("native stdout is one JSON object")
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let ty = entry.file_type().unwrap();
        let dst = to.join(entry.file_name());
        if ty.is_dir() {
            copy_tree(&entry.path(), &dst);
        } else if ty.is_file() {
            fs::copy(entry.path(), dst).unwrap();
        }
    }
}

fn stats(samples: &mut [Duration]) -> (f64, f64) {
    samples.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    (ms(samples[0]), ms(samples[samples.len() / 2]))
}

#[test]
#[ignore = "needs CODE_GRAPH_NATIVE_BIN and CODE_GRAPH_CORPUS"]
fn corpus_index_matches_native_and_is_timed() {
    let bin = PathBuf::from(std::env::var("CODE_GRAPH_NATIVE_BIN").expect("CODE_GRAPH_NATIVE_BIN"));
    let corpus = PathBuf::from(std::env::var("CODE_GRAPH_CORPUS").expect("CODE_GRAPH_CORPUS"))
        .canonicalize()
        .unwrap();
    let rounds: usize = std::env::var("CODE_GRAPH_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7);
    let name = corpus.file_name().unwrap().to_str().unwrap().to_string();
    let work = corpus.parent().unwrap().join("wasm");
    let copy = work.join(&name);
    if !copy.exists() {
        copy_tree(&corpus, &copy);
    }
    for repo in [&corpus, &copy] {
        let _ = fs::remove_dir_all(repo.join(".murmur"));
    }
    let abs = corpus.to_str().unwrap().to_string();
    let n_index = json!({"operation": "index_repository", "repo_path": abs});
    let w_index = json!({"operation": "index_repository", "repo_path": name});

    // Parity: one full index each.
    let n = native(&bin, &corpus, &n_index);
    let w = call(&work, &[], &w_index);
    assert_eq!(n["status"], "passed", "{n}");
    assert_eq!(w.status, "passed", "{w}");
    for key in [
        "changed_files",
        "unchanged_files",
        "removed_files",
        "total_symbols",
        "total_edges",
    ] {
        assert_eq!(n["data"][key], w.data[key], "{key}");
    }
    let n_rows = snapshot(&corpus.join(".murmur/code-graph.db"));
    let w_rows = snapshot(&copy.join(".murmur/code-graph.db"));
    for table in ["files", "symbols", "edges", "symbols_fts"] {
        let count = n_rows[table].as_object().unwrap().len();
        eprintln!(
            "{table}: {count} rows native, {} rows component",
            w_rows[table].as_object().unwrap().len()
        );
    }
    assert!(n_rows == w_rows, "the corpus databases differ");
    eprintln!("snapshots identical: {}", n["summary"]);

    // The largest function by line span, for impact_analysis.
    let (file, start, end) = n_rows["symbols"]
        .as_object()
        .unwrap()
        .values()
        .filter(|s| s["kind"] == "function")
        .map(|s| {
            (
                s["file"].as_str().unwrap().to_string(),
                s["start_line"].as_i64().unwrap(),
                s["end_line"].as_i64().unwrap(),
            )
        })
        .max_by_key(|(f, a, b)| (b - a, std::cmp::Reverse(f.clone())))
        .unwrap();
    eprintln!("impact_analysis over {file}:{start}-{end}");
    let find = |repo: &str| json!({"operation": "find_symbol", "repo_path": repo, "query": "run"});
    let impact = |repo: &str| {
        json!({"operation": "impact_analysis", "repo_path": repo, "file": file,
               "start_line": start, "end_line": end})
    };
    let n_find = native(&bin, &corpus, &find(&abs));
    let w_find = call(&work, &[], &find(&name));
    assert_eq!(n_find["data"], w_find.data);
    let n_impact = native(&bin, &corpus, &impact(&abs));
    let w_impact: Outcome = call(&work, &[], &impact(&name));
    let rewrite = |v: &Value| v.to_string().replace(&abs, &name);
    assert_eq!(rewrite(&n_impact["data"]), w_impact.data.to_string());

    // Timing: interleaved rounds.
    type Op<'a> = (&'a str, bool, Value, Value);
    let ops: Vec<Op> = vec![
        (
            "index from no database",
            true,
            n_index.clone(),
            w_index.clone(),
        ),
        ("no-op re-index", false, n_index.clone(), w_index.clone()),
        ("find_symbol run", false, find(&abs), find(&name)),
        ("impact_analysis", false, impact(&abs), impact(&name)),
    ];
    // Optionally a second build of the component (the S10 opt-level candidate), timed in the
    // same interleaved rounds.
    let b = std::env::var_os("CODE_GRAPH_WASM_B").map(PathBuf::from);
    if let Some(b) = &b {
        let out = call_component(b, &work, &find(&name));
        assert_eq!(out.data, w_find.data, "the B build answers differently");
    }
    eprintln!(
        "| operation | native min / median | component min / median | ratio (median) | B min / median | B / component (median) |"
    );
    eprintln!("|---|---|---|---|---|---|");
    for (label, fresh, n_args, w_args) in ops {
        let (mut ns, mut ws, mut bs) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..rounds {
            if fresh {
                fs::remove_dir_all(corpus.join(".murmur")).unwrap();
            }
            let t = Instant::now();
            let out = native(&bin, &corpus, &n_args);
            ns.push(t.elapsed());
            assert_eq!(out["status"], "passed");
            if fresh {
                fs::remove_dir_all(copy.join(".murmur")).unwrap();
            }
            let t = Instant::now();
            let out = call(&work, &[], &w_args);
            ws.push(t.elapsed());
            assert_eq!(out.status, "passed");
            if let Some(b) = &b {
                if fresh {
                    fs::remove_dir_all(copy.join(".murmur")).unwrap();
                }
                let t = Instant::now();
                let out = call_component(b, &work, &w_args);
                bs.push(t.elapsed());
                assert_eq!(out.status, "passed");
            }
        }
        let (n_min, n_med) = stats(&mut ns);
        let (w_min, w_med) = stats(&mut ws);
        let b_cols = if bs.is_empty() {
            "– | –".to_string()
        } else {
            let (b_min, b_med) = stats(&mut bs);
            format!("{b_min:.0} / {b_med:.0} ms | {:.3}×", b_med / w_med)
        };
        eprintln!(
            "| {label} | {n_min:.0} / {n_med:.0} ms | {w_min:.0} / {w_med:.0} ms | {:.2}× | {b_cols} |",
            w_med / n_med
        );
    }
    eprintln!(
        "load: {}",
        fs::read_to_string("/proc/loadavg").unwrap().trim()
    );
}
