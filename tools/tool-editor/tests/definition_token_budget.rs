//! What the tool's definition costs on every model call.
//!
//! murmur sends a tool to the model as `{name, description, parameters}`: the manifest
//! `description`, trimmed, and `input_schema` converted with `serde_json::to_value` and sent
//! verbatim (`crates/capsule-runtime/src/agent/inventory.rs`). It counts tokens with
//! `tiktoken-rs`'s `cl100k_base` (`agent.rs`, `count_tokens`). This test counts the description
//! and the compact schema JSON the same way and holds their sum to a budget.
//!
//! `count_definition_file` is `#[ignore]`d: it measures another definition, a manifest variant
//! or a tool entry cut from a logged request, named by `TOOL_EDITOR_COUNT`:
//!
//! ```text
//! TOOL_EDITOR_COUNT=<file> cargo test -p tool-editor --test definition_token_budget \
//!     count_definition_file -- --ignored --nocapture
//! ```

use std::{fs, path::Path, path::PathBuf};

use serde_json::Value;
use tiktoken_rs::CoreBPE;

/// The most cl100k tokens the shipped description and schema may cost together.
const BUDGET: usize = 500;

fn tokens(bpe: &CoreBPE, text: &str) -> usize {
    bpe.encode_with_special_tokens(text).len()
}

/// A definition's two parts as murmur sends them: the trimmed description and the compact
/// schema JSON.
struct Definition {
    description: String,
    schema: String,
}

impl Definition {
    /// From a `murmur.yaml`'s `description` and `input_schema`.
    fn from_manifest(text: &str) -> Self {
        let manifest: serde_norway::Value =
            serde_norway::from_str(text).expect("the manifest parses as YAML");
        let description = manifest
            .get("description")
            .and_then(serde_norway::Value::as_str)
            .expect("the manifest has a string description")
            .trim()
            .to_string();
        let schema = serde_json::to_value(
            manifest
                .get("input_schema")
                .expect("the manifest has an input_schema"),
        )
        .expect("input_schema converts to JSON")
        .to_string();
        Definition {
            description,
            schema,
        }
    }

    /// From one tool entry carrying `description` and `parameters`.
    fn from_entry(entry: &Value) -> Self {
        let description = entry["description"]
            .as_str()
            .expect("the entry has a string description")
            .trim()
            .to_string();
        assert!(
            entry["parameters"].is_object(),
            "the entry has object parameters"
        );
        Definition {
            description,
            schema: entry["parameters"].to_string(),
        }
    }

    /// `(description chars, description tokens, schema chars, schema tokens)`.
    fn measure(&self, bpe: &CoreBPE) -> (usize, usize, usize, usize) {
        (
            self.description.chars().count(),
            tokens(bpe, &self.description),
            self.schema.chars().count(),
            tokens(bpe, &self.schema),
        )
    }

    /// One line per part and their total.
    fn breakdown(&self, bpe: &CoreBPE) -> String {
        let (d_chars, d_tokens, s_chars, s_tokens) = self.measure(bpe);
        format!(
            "description: {d_chars} chars, {d_tokens} tokens\n\
             schema:      {s_chars} chars, {s_tokens} tokens\n\
             total:       {} chars, {} tokens",
            d_chars + s_chars,
            d_tokens + s_tokens
        )
    }
}

fn bpe() -> CoreBPE {
    tiktoken_rs::cl100k_base().expect("the cl100k_base encoding loads")
}

#[test]
fn the_shipped_definition_costs_at_most_the_budget() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("murmur.yaml");
    let definition = Definition::from_manifest(&fs::read_to_string(&path).unwrap());
    let bpe = bpe();
    let (_, d_tokens, _, s_tokens) = definition.measure(&bpe);
    let breakdown = definition.breakdown(&bpe);
    println!("{breakdown}");
    assert!(
        d_tokens + s_tokens <= BUDGET,
        "tool-editor's definition costs more than {BUDGET} cl100k tokens on every model call:\n\
         {breakdown}"
    );
}

#[test]
#[ignore = "measures the file TOOL_EDITOR_COUNT names; run with --ignored --nocapture"]
fn count_definition_file() {
    let path = std::env::var_os("TOOL_EDITOR_COUNT")
        .map(PathBuf::from)
        .expect("set TOOL_EDITOR_COUNT to a murmur.yaml or a tool entry *.json");
    let text =
        fs::read_to_string(&path).unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
    let bpe = bpe();
    println!("{}", path.display());
    if Path::new(&path)
        .extension()
        .is_some_and(|ext| ext == "json")
    {
        let entry: Value = serde_json::from_str(&text).expect("the entry is JSON");
        println!("{}", Definition::from_entry(&entry).breakdown(&bpe));
        let whole = entry.to_string();
        println!(
            "entry:       {} chars, {} tokens",
            whole.chars().count(),
            tokens(&bpe, &whole)
        );
    } else {
        println!("{}", Definition::from_manifest(&text).breakdown(&bpe));
    }
}
