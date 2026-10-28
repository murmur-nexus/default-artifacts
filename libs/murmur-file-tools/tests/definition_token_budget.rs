//! What the four file tools' definitions cost together on every model call.
//!
//! murmur sends each tool to the model as `{name, description, parameters}`: the manifest
//! `description`, trimmed, and `input_schema` converted with `serde_json::to_value` and sent
//! verbatim (`crates/capsule-runtime/src/agent/inventory.rs`). It counts tokens with
//! `tiktoken-rs`'s `cl100k_base` (`agent.rs`, `count_tokens`). This test counts each tool's
//! description and compact schema JSON the same way and holds the sum over the four to a budget.
//! A driver's wrapper keys around each entry are outside the budget.
//!
//! `count_definition_file` is `#[ignore]`d: it measures the file `FILE_TOOLS_COUNT` names — a
//! `murmur.yaml`, one tool entry `*.json`, or a JSON array of tool entries such as a logged
//! request's `tools`:
//!
//! ```text
//! FILE_TOOLS_COUNT=<file> cargo test -p murmur-file-tools --test definition_token_budget \
//!     count_definition_file -- --ignored --nocapture
//! ```

use std::{fs, path::PathBuf};

use murmur_file_tools::Tool;
use serde_json::Value;
use tiktoken_rs::CoreBPE;

/// The most cl100k tokens the four shipped descriptions and schemas may cost together.
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

/// `(description chars, description tokens, schema chars, schema tokens)`.
#[derive(Clone, Copy, Default)]
struct Measure {
    d_chars: usize,
    d_tokens: usize,
    s_chars: usize,
    s_tokens: usize,
}

impl Measure {
    fn chars(self) -> usize {
        self.d_chars + self.s_chars
    }

    fn tokens(self) -> usize {
        self.d_tokens + self.s_tokens
    }

    fn add(self, other: Measure) -> Measure {
        Measure {
            d_chars: self.d_chars + other.d_chars,
            d_tokens: self.d_tokens + other.d_tokens,
            s_chars: self.s_chars + other.s_chars,
            s_tokens: self.s_tokens + other.s_tokens,
        }
    }

    /// One table row: description, schema and total, each in chars and tokens.
    fn row(self, label: &str) -> String {
        format!(
            "{label:<12} description {:>4} chars {:>3} tokens | schema {:>4} chars {:>3} tokens | \
             total {:>4} chars {:>3} tokens",
            self.d_chars,
            self.d_tokens,
            self.s_chars,
            self.s_tokens,
            self.chars(),
            self.tokens()
        )
    }
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

    /// From one tool entry carrying `description` and `parameters` (or `input_schema`), bare or
    /// under a Chat Completions `function` wrapper.
    fn from_entry(entry: &Value) -> Self {
        let inner = entry.get("function").unwrap_or(entry);
        let description = inner["description"]
            .as_str()
            .expect("the entry has a string description")
            .trim()
            .to_string();
        let schema = inner
            .get("parameters")
            .or_else(|| inner.get("input_schema"))
            .filter(|schema| schema.is_object())
            .expect("the entry has object parameters");
        Definition {
            description,
            schema: schema.to_string(),
        }
    }

    fn measure(&self, bpe: &CoreBPE) -> Measure {
        Measure {
            d_chars: self.description.chars().count(),
            d_tokens: tokens(bpe, &self.description),
            s_chars: self.schema.chars().count(),
            s_tokens: tokens(bpe, &self.schema),
        }
    }
}

/// An entry's tool name, bare or under a `function` wrapper.
fn entry_name(entry: &Value) -> &str {
    entry
        .get("function")
        .unwrap_or(entry)
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("?")
}

fn bpe() -> CoreBPE {
    tiktoken_rs::cl100k_base().expect("the cl100k_base encoding loads")
}

#[test]
fn the_four_definitions_cost_at_most_the_budget() {
    let bpe = bpe();
    let mut total = Measure::default();
    let mut table = Vec::new();
    for tool in Tool::ALL {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tools")
            .join(tool.name())
            .join("murmur.yaml");
        let measure = Definition::from_manifest(&fs::read_to_string(&path).unwrap()).measure(&bpe);
        table.push(measure.row(tool.name()));
        total = total.add(measure);
    }
    table.push(total.row("sum"));
    let table = table.join("\n");
    println!("{table}");
    assert!(
        total.tokens() <= BUDGET,
        "the four file tools' definitions cost more than {BUDGET} cl100k tokens on every model \
         call:\n{table}"
    );
}

#[test]
#[ignore = "measures the file FILE_TOOLS_COUNT names; run with --ignored --nocapture"]
fn count_definition_file() {
    let path = std::env::var_os("FILE_TOOLS_COUNT")
        .map(PathBuf::from)
        .expect(
            "set FILE_TOOLS_COUNT to a murmur.yaml, a tool entry *.json or a JSON array of them",
        );
    let text =
        fs::read_to_string(&path).unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
    let bpe = bpe();
    println!("{}", path.display());
    if path.extension().is_none_or(|ext| ext != "json") {
        println!(
            "{}",
            Definition::from_manifest(&text)
                .measure(&bpe)
                .row("definition")
        );
        return;
    }
    let value: Value = serde_json::from_str(&text).expect("the file is JSON");
    let entries: Vec<&Value> = match &value {
        Value::Array(entries) => entries.iter().collect(),
        entry => vec![entry],
    };
    let mut total = Measure::default();
    let (mut whole_chars, mut whole_tokens) = (0, 0);
    for entry in &entries {
        let measure = Definition::from_entry(entry).measure(&bpe);
        let whole = entry.to_string();
        let (chars, tokens) = (whole.chars().count(), tokens(&bpe, &whole));
        println!(
            "{} | entry {chars:>4} chars {tokens:>3} tokens",
            measure.row(entry_name(entry))
        );
        total = total.add(measure);
        whole_chars += chars;
        whole_tokens += tokens;
    }
    if entries.len() > 1 {
        println!(
            "{} | entry {whole_chars:>4} chars {whole_tokens:>3} tokens",
            total.row("sum")
        );
        let array = value.to_string();
        println!(
            "array:       {} chars, {} tokens",
            array.chars().count(),
            tokens(&bpe, &array)
        );
    }
}
