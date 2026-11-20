//! What the agent may ask for: one of five operations, selected by `op`, and the properties
//! that operation uses.
//!
//! The model's `input` arrives verbatim and unvalidated as `tool-input.data`, so every rule in
//! `input_schema` is enforced again here. A JSON `null` counts as absent; a non-null property
//! the op does not use is refused, naming it and the op. Every rule here runs before any
//! request, so a refused call sends nothing.

use serde_json::{Map, Value};

/// The longest search query, in characters, after trimming.
pub const MAX_QUERY_CHARS: usize = 200;
/// The longest `task`, `library`, `license` or `tag`, in characters.
pub const MAX_FILTER_CHARS: usize = 100;
/// The longest `where` or `order_by`, in characters.
pub const MAX_WHERE_CHARS: usize = 1000;
/// The longest `repo`, in bytes.
pub const MAX_REPO_BYTES: usize = 96;
/// The longest `revision`, in bytes.
pub const MAX_REVISION_BYTES: usize = 128;
/// The longest `repo_file`, in bytes.
pub const MAX_REPO_FILE_BYTES: usize = 1024;
/// The longest `dest`, in bytes.
pub const MAX_DEST_BYTES: usize = 1024;
/// The longest single `dest` component, in bytes.
pub const MAX_DEST_COMPONENT_BYTES: usize = 255;
/// The longest `config` or `split` name, in bytes.
pub const MAX_SPLIT_NAME_BYTES: usize = 200;

/// `limit` bounds for `search`.
pub const SEARCH_LIMIT_DEFAULT: u32 = 10;
pub const SEARCH_LIMIT_MAX: u32 = 50;
/// `limit` bounds for `dataset`.
pub const DATASET_LIMIT_DEFAULT: u32 = 10;
pub const DATASET_LIMIT_MAX: u32 = 100;

/// The revision used when the agent names none.
pub const DEFAULT_REVISION: &str = "main";

/// The operation a call selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Search,
    Repo,
    Dataset,
    Download,
    Paper,
}

impl Op {
    const ALL: &[(&'static str, Self)] = &[
        ("search", Self::Search),
        ("repo", Self::Repo),
        ("dataset", Self::Dataset),
        ("download", Self::Download),
        ("paper", Self::Paper),
    ];

    pub fn as_str(self) -> &'static str {
        name_of(Self::ALL, self)
    }

    /// The properties this op reads, besides `op` itself.
    fn properties(self) -> &'static [&'static str] {
        match self {
            Self::Search => &[
                "kind", "query", "task", "library", "license", "tag", "sort", "limit",
            ],
            Self::Repo => &["kind", "repo", "revision"],
            Self::Dataset => &[
                "repo", "config", "split", "query", "where", "order_by", "limit", "offset",
            ],
            Self::Download => &[
                "kind",
                "repo",
                "revision",
                "repo_file",
                "dest",
                "byte_offset",
                "byte_length",
            ],
            Self::Paper => &["arxiv_id"],
        }
    }
}

/// What a `kind` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Model,
    Dataset,
    Space,
    Paper,
}

impl Kind {
    const ALL: &[(&'static str, Self)] = &[
        ("model", Self::Model),
        ("dataset", Self::Dataset),
        ("space", Self::Space),
        ("paper", Self::Paper),
    ];

    pub fn as_str(self) -> &'static str {
        name_of(Self::ALL, self)
    }

    /// The plural the Hub API uses in `/api/<plural>`.
    pub fn plural(self) -> &'static str {
        match self {
            Self::Model => "models",
            Self::Dataset => "datasets",
            Self::Space => "spaces",
            Self::Paper => "papers",
        }
    }

    /// The prefix of a repo's web path: none for a model, `datasets/` or `spaces/` otherwise.
    pub fn repo_prefix(self) -> &'static str {
        match self {
            Self::Dataset => "datasets/",
            Self::Space => "spaces/",
            Self::Model | Self::Paper => "",
        }
    }
}

/// The search order, always descending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Downloads,
    Likes,
    Updated,
    Trending,
    Created,
}

impl Sort {
    const ALL: &[(&'static str, Self)] = &[
        ("downloads", Self::Downloads),
        ("likes", Self::Likes),
        ("updated", Self::Updated),
        ("trending", Self::Trending),
        ("created", Self::Created),
    ];

    pub fn as_str(self) -> &'static str {
        name_of(Self::ALL, self)
    }

    /// The Hub's own `sort` field name.
    pub fn hub_field(self) -> &'static str {
        match self {
            Self::Downloads => "downloads",
            Self::Likes => "likes",
            Self::Updated => "lastModified",
            Self::Trending => "trendingScore",
            Self::Created => "createdAt",
        }
    }
}

fn name_of<T: PartialEq + Copy>(table: &[(&'static str, T)], value: T) -> &'static str {
    table
        .iter()
        .find(|(_, candidate)| *candidate == value)
        .map(|(name, _)| *name)
        .expect("every variant is in its table")
}

/// A result count: the value used, and what the agent asked for when it was clamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub value: u32,
    /// The agent's own value when it was above the maximum and clamped to `value`.
    pub clamped_from: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchInput {
    pub kind: Kind,
    pub query: String,
    pub task: Option<String>,
    pub library: Option<String>,
    pub license: Option<String>,
    pub tag: Option<String>,
    pub sort: Option<Sort>,
    pub limit: Limit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInput {
    pub kind: Kind,
    pub repo: String,
    /// The agent's revision; `None` reads the default branch, shown as `main`.
    pub revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatasetInput {
    pub repo: String,
    pub config: Option<String>,
    pub split: Option<String>,
    pub query: Option<String>,
    pub where_: Option<String>,
    pub order_by: Option<String>,
    pub limit: Limit,
    pub offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadInput {
    pub kind: Kind,
    pub repo: String,
    pub revision: String,
    pub repo_file: String,
    pub dest: String,
    /// `(byte_offset, byte_length)` of a ranged download.
    pub range: Option<(u64, u64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaperInput {
    /// The arXiv id without a version suffix.
    pub arxiv_id: String,
}

/// A validated call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Search(SearchInput),
    Repo(RepoInput),
    Dataset(DatasetInput),
    Download(DownloadInput),
    Paper(PaperInput),
}

impl Input {
    pub fn op(&self) -> Op {
        match self {
            Self::Search(_) => Op::Search,
            Self::Repo(_) => Op::Repo,
            Self::Dataset(_) => Op::Dataset,
            Self::Download(_) => Op::Download,
            Self::Paper(_) => Op::Paper,
        }
    }
}

/// Parse the tool input. The `Err` string is the agent-facing `invalid_input` message.
///
/// The input is a JSON object, or the same object encoded twice (re-parsed once).
/// `max_download_bytes` bounds `byte_length`.
pub fn parse_input(data: &str, max_download_bytes: u64) -> Result<Input, String> {
    let trimmed = data.trim();
    if trimmed.is_empty() {
        return Err("no input; expected {\"op\": \"search\" | \"repo\" | \"dataset\" | \"download\" | \"paper\", ...}".to_string());
    }
    let parsed: Value =
        serde_json::from_str(trimmed).map_err(|e| format!("input is not valid JSON: {e}"))?;
    let mut args = match parsed {
        Value::Object(map) => map,
        Value::String(inner) => match serde_json::from_str::<Value>(&inner) {
            Ok(Value::Object(map)) => map,
            _ => return Err("input must be a JSON object with an \"op\"".to_string()),
        },
        other => {
            return Err(format!(
                "input must be a JSON object with an \"op\", got {}",
                json_type_name(&other)
            ))
        }
    };
    // A null counts as absent.
    args.retain(|_, value| !value.is_null());

    let op = match args.remove("op") {
        None => return Err("\"op\" is required: search, repo, dataset, download or paper".into()),
        Some(value) => enum_value("op", &value, Op::ALL)?,
    };
    let known = [
        "kind",
        "query",
        "task",
        "library",
        "license",
        "tag",
        "sort",
        "limit",
        "offset",
        "repo",
        "revision",
        "repo_file",
        "dest",
        "byte_offset",
        "byte_length",
        "config",
        "split",
        "where",
        "order_by",
        "arxiv_id",
    ];
    // Sorted so the message names the same key whatever order the model wrote them in.
    let mut keys: Vec<&String> = args.keys().collect();
    keys.sort();
    if let Some(key) = keys.iter().find(|key| !known.contains(&key.as_str())) {
        return Err(format!("'{key}' is not an input of this tool"));
    }
    if let Some(key) = keys
        .iter()
        .find(|key| !op.properties().contains(&key.as_str()))
    {
        return Err(format!(
            "'{key}' is not used by op {}; it takes {}",
            op.as_str(),
            op.properties().join(", ")
        ));
    }

    let mut args = Args { map: args, op };
    match op {
        Op::Search => search(&mut args).map(Input::Search),
        Op::Repo => repo_input(&mut args).map(Input::Repo),
        Op::Dataset => dataset(&mut args).map(Input::Dataset),
        Op::Download => download(&mut args, max_download_bytes).map(Input::Download),
        Op::Paper => paper(&mut args).map(Input::Paper),
    }
}

struct Args {
    map: Map<String, Value>,
    op: Op,
}

impl Args {
    fn string(&mut self, key: &str) -> Result<Option<String>, String> {
        match self.map.remove(key) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text)),
            Some(other) => Err(format!(
                "\"{key}\" must be a string, got {}",
                json_type_name(&other)
            )),
        }
    }

    fn required_string(&mut self, key: &str) -> Result<String, String> {
        self.string(key)?
            .ok_or_else(|| format!("\"{key}\" is required by op {}", self.op.as_str()))
    }

    fn integer(&mut self, key: &str) -> Result<Option<i128>, String> {
        match self.map.remove(key) {
            None => Ok(None),
            Some(value) => match value
                .as_i64()
                .map(i128::from)
                .or_else(|| value.as_u64().map(i128::from))
            {
                Some(n) => Ok(Some(n)),
                None => Err(format!("\"{key}\" must be an integer, got {value}")),
            },
        }
    }

    fn kind(&mut self, default: Option<Kind>) -> Result<Kind, String> {
        match self.map.remove("kind") {
            None => default.ok_or_else(|| {
                format!(
                    "\"kind\" is required by op {}: model, dataset, space or paper",
                    self.op.as_str()
                )
            }),
            Some(value) => enum_value("kind", &value, Kind::ALL),
        }
    }
}

fn search(args: &mut Args) -> Result<SearchInput, String> {
    let kind = args.kind(None)?;
    let query = query(args.required_string("query")?)?;
    let mut filter = |key: &str| -> Result<Option<String>, String> {
        let Some(value) = args.string(key)? else {
            return Ok(None);
        };
        if !matches!(kind, Kind::Model | Kind::Dataset) {
            return Err(format!(
                "\"{key}\" applies only to kind model or dataset, not {}",
                kind.as_str()
            ));
        }
        let value = value.trim().to_string();
        if value.is_empty() {
            return Err(format!("\"{key}\" must not be blank"));
        }
        let chars = value.chars().count();
        if chars > MAX_FILTER_CHARS {
            return Err(format!(
                "\"{key}\" is {chars} characters; at most {MAX_FILTER_CHARS} are accepted"
            ));
        }
        if value.chars().any(char::is_control) {
            return Err(format!("\"{key}\" must not contain control characters"));
        }
        Ok(Some(value))
    };
    let task = filter("task")?;
    let library = filter("library")?;
    let license = filter("license")?;
    let tag = filter("tag")?;
    let sort = match args.map.remove("sort") {
        None => None,
        Some(value) => {
            if !matches!(kind, Kind::Model | Kind::Dataset) {
                return Err(format!(
                    "\"sort\" applies only to kind model or dataset, not {}",
                    kind.as_str()
                ));
            }
            Some(enum_value("sort", &value, Sort::ALL)?)
        }
    };
    let limit = limit(args, SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX)?;
    Ok(SearchInput {
        kind,
        query,
        task,
        library,
        license,
        tag,
        sort,
        limit,
    })
}

fn repo_input(args: &mut Args) -> Result<RepoInput, String> {
    let kind = repo_kind(args)?;
    let repo = repo(args.required_string("repo")?)?;
    let revision = args.string("revision")?.map(revision).transpose()?;
    Ok(RepoInput {
        kind,
        repo,
        revision,
    })
}

fn dataset(args: &mut Args) -> Result<DatasetInput, String> {
    let repo = repo(args.required_string("repo")?)?;
    let config = args
        .string("config")?
        .map(|v| split_name("config", v))
        .transpose()?;
    let split = args
        .string("split")?
        .map(|v| split_name("split", v))
        .transpose()?;
    let query = args.string("query")?.map(query).transpose()?;
    let where_ = args
        .string("where")?
        .map(|v| where_text("where", v))
        .transpose()?;
    let order_by = args
        .string("order_by")?
        .map(|v| where_text("order_by", v))
        .transpose()?;
    if where_.is_some() && query.is_some() {
        return Err(
            "\"where\" and \"query\" cannot be combined: a filter or a text search, not both"
                .into(),
        );
    }
    if order_by.is_some() && where_.is_none() {
        return Err(
            "\"order_by\" needs \"where\": the datasets server orders only filtered rows".into(),
        );
    }
    let limit = limit(args, DATASET_LIMIT_DEFAULT, DATASET_LIMIT_MAX)?;
    let offset = match args.integer("offset")? {
        None => 0,
        Some(n) if n >= 0 && n <= i128::from(u32::MAX) => n as u64,
        Some(n) => {
            return Err(format!(
                "\"offset\" must be an integer of at least 0, got {n}"
            ))
        }
    };
    Ok(DatasetInput {
        repo,
        config,
        split,
        query,
        where_,
        order_by,
        limit,
        offset,
    })
}

fn download(args: &mut Args, max_download_bytes: u64) -> Result<DownloadInput, String> {
    let kind = repo_kind(args)?;
    let repo = repo(args.required_string("repo")?)?;
    let revision = args
        .string("revision")?
        .map(revision)
        .transpose()?
        .unwrap_or_else(|| DEFAULT_REVISION.to_string());
    let repo_file = repo_file(args.required_string("repo_file")?)?;
    let dest = dest(args.required_string("dest")?)?;
    let offset = args.integer("byte_offset")?;
    let length = args.integer("byte_length")?;
    let range = match (offset, length) {
        (None, None) => None,
        (Some(_), None) | (None, Some(_)) => {
            return Err(
                "\"byte_offset\" and \"byte_length\" go together: give both or neither".into(),
            )
        }
        (Some(offset), Some(length)) => {
            if offset < 0 {
                return Err(format!("\"byte_offset\" must be at least 0, got {offset}"));
            }
            if length < 1 || length > i128::from(max_download_bytes) {
                return Err(format!(
                    "\"byte_length\" must be from 1 to {max_download_bytes} (the operator's \
                     max_download_bytes), got {length}"
                ));
            }
            if offset > i128::from(u64::MAX / 2) {
                return Err(format!("\"byte_offset\" is out of range: {offset}"));
            }
            Some((offset as u64, length as u64))
        }
    };
    Ok(DownloadInput {
        kind,
        repo,
        revision,
        repo_file,
        dest,
        range,
    })
}

fn paper(args: &mut Args) -> Result<PaperInput, String> {
    let id = args.required_string("arxiv_id")?;
    Ok(PaperInput {
        arxiv_id: arxiv_id(&id)?,
    })
}

fn repo_kind(args: &mut Args) -> Result<Kind, String> {
    let kind = args.kind(Some(Kind::Model))?;
    if kind == Kind::Paper {
        return Err(format!(
            "kind paper has no repository; op {} takes kind model, dataset or space (use op paper \
             for a paper)",
            args.op.as_str()
        ));
    }
    Ok(kind)
}

fn limit(args: &mut Args, default: u32, max: u32) -> Result<Limit, String> {
    match args.integer("limit")? {
        None => Ok(Limit {
            value: default,
            clamped_from: None,
        }),
        Some(n) if n < 1 => Err(format!("\"limit\" must be at least 1, got {n}")),
        Some(n) if n > i128::from(max) => Ok(Limit {
            value: max,
            clamped_from: Some(u64::try_from(n).unwrap_or(u64::MAX)),
        }),
        Some(n) => Ok(Limit {
            value: n as u32,
            clamped_from: None,
        }),
    }
}

fn query(value: String) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("\"query\" must not be blank".into());
    }
    let chars = value.chars().count();
    if chars > MAX_QUERY_CHARS {
        return Err(format!(
            "\"query\" is {chars} characters; at most {MAX_QUERY_CHARS} are accepted"
        ));
    }
    if value.chars().any(char::is_control) {
        return Err("\"query\" must not contain control characters".into());
    }
    Ok(value.to_string())
}

fn where_text(key: &str, value: String) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("\"{key}\" must not be blank"));
    }
    let chars = value.chars().count();
    if chars > MAX_WHERE_CHARS {
        return Err(format!(
            "\"{key}\" is {chars} characters; at most {MAX_WHERE_CHARS} are accepted"
        ));
    }
    if value
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(format!("\"{key}\" must not contain control characters"));
    }
    Ok(value.to_string())
}

fn split_name(key: &str, value: String) -> Result<String, String> {
    if value.is_empty() || value.len() > MAX_SPLIT_NAME_BYTES {
        return Err(format!(
            "\"{key}\" must be 1 to {MAX_SPLIT_NAME_BYTES} bytes, got {} bytes",
            value.len()
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("\"{key}\" must not contain control characters"));
    }
    Ok(value)
}

/// `owner/name` or a bare legacy `name`. Each segment is `[A-Za-z0-9][A-Za-z0-9._-]*` with no
/// `..` or `--`; the whole is at most [`MAX_REPO_BYTES`].
pub fn repo(value: String) -> Result<String, String> {
    let value = value.trim().to_string();
    if value.is_empty() || value.len() > MAX_REPO_BYTES {
        return Err(format!(
            "\"repo\" must be owner/name, 1 to {MAX_REPO_BYTES} characters, got '{value}'"
        ));
    }
    let segments: Vec<&str> = value.split('/').collect();
    if segments.len() > 2 {
        return Err(format!(
            "\"repo\" must be owner/name or name, got '{value}'"
        ));
    }
    for segment in &segments {
        let mut bytes = segment.bytes();
        let first_ok = bytes.next().is_some_and(|b| b.is_ascii_alphanumeric());
        let rest_ok = segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if !first_ok || !rest_ok || segment.contains("..") || segment.contains("--") {
            return Err(format!(
                "\"repo\" must be owner/name, each part starting with a letter or digit and \
                 holding only letters, digits, '.', '_' and '-' (no '..' or '--'), got '{value}'"
            ));
        }
    }
    Ok(value)
}

/// `[A-Za-z0-9._/-]`, no `..`, 1 to [`MAX_REVISION_BYTES`] bytes.
pub fn revision(value: String) -> Result<String, String> {
    let value = value.trim().to_string();
    let ok = !value.is_empty()
        && value.len() <= MAX_REVISION_BYTES
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-'))
        && !value.contains("..");
    if !ok {
        return Err(format!(
            "\"revision\" must be a branch, tag or commit: 1 to {MAX_REVISION_BYTES} of letters, \
             digits, '.', '_', '/' and '-', with no '..', got '{value}'"
        ));
    }
    Ok(value)
}

/// A path inside the repository: relative, no empty, `.` or `..` component, no `\`, no control
/// characters.
pub fn repo_file(value: String) -> Result<String, String> {
    if value.is_empty() || value.len() > MAX_REPO_FILE_BYTES {
        return Err(format!(
            "\"repo_file\" must be 1 to {MAX_REPO_FILE_BYTES} bytes, got {}",
            value.len()
        ));
    }
    if value.starts_with('/') {
        return Err(format!(
            "\"repo_file\" is a path inside the repository and must not start with '/', got '{value}'"
        ));
    }
    if value.contains('\\') || value.chars().any(char::is_control) {
        return Err(format!(
            "\"repo_file\" must not contain '\\' or control characters, got {value:?}"
        ));
    }
    if value
        .split('/')
        .any(|component| matches!(component, "" | "." | ".."))
    {
        return Err(format!(
            "\"repo_file\" must have no empty, '.' or '..' component, got '{value}'"
        ));
    }
    Ok(value)
}

/// A path relative to the work folder. The existence check is the caller's, since it needs the
/// work folder.
pub fn dest(value: String) -> Result<String, String> {
    if value.is_empty() {
        return Err("\"dest\" must not be empty".into());
    }
    if value.len() > MAX_DEST_BYTES {
        return Err(format!(
            "\"dest\" is {} bytes; at most {MAX_DEST_BYTES} are accepted",
            value.len()
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(format!(
            "\"dest\" must not contain NUL or control characters, got {value:?}"
        ));
    }
    if value.contains('\\') {
        return Err(format!("\"dest\" must not contain '\\', got '{value}'"));
    }
    let bytes = value.as_bytes();
    if value.starts_with('/')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
    {
        return Err(format!(
            "\"dest\" must be relative to the work folder, got the absolute path '{value}'"
        ));
    }
    if value.ends_with('/') {
        return Err(format!(
            "\"dest\" must name a file, not a directory (no trailing '/'), got '{value}'"
        ));
    }
    for component in value.split('/') {
        if matches!(component, "" | "." | "..") {
            return Err(format!(
                "\"dest\" must have no empty, '.' or '..' component, got '{value}'"
            ));
        }
        if component.len() > MAX_DEST_COMPONENT_BYTES {
            return Err(format!(
                "\"dest\" has a component of {} bytes; at most {MAX_DEST_COMPONENT_BYTES} are \
                 accepted",
                component.len()
            ));
        }
    }
    Ok(value)
}

/// `^\d{4}\.\d{4,5}(v\d+)?$`, returned without its version suffix.
pub fn arxiv_id(value: &str) -> Result<String, String> {
    let value = value.trim();
    let base = match value.find('v') {
        Some(at) => {
            let version = &value[at + 1..];
            if version.is_empty() || !version.bytes().all(|b| b.is_ascii_digit()) {
                return Err(arxiv_message(value));
            }
            &value[..at]
        }
        None => value,
    };
    let ok = match base.split_once('.') {
        Some((year, number)) => {
            year.len() == 4
                && year.bytes().all(|b| b.is_ascii_digit())
                && (4..=5).contains(&number.len())
                && number.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    };
    if !ok {
        return Err(arxiv_message(value));
    }
    Ok(base.to_string())
}

fn arxiv_message(value: &str) -> String {
    format!("\"arxiv_id\" must be an arXiv id such as 1706.03762 (a version suffix such as v2 is allowed), got '{value}'")
}

fn enum_value<T: Copy>(key: &str, value: &Value, table: &[(&'static str, T)]) -> Result<T, String> {
    value
        .as_str()
        .and_then(|name| table.iter().find(|(candidate, _)| *candidate == name))
        .map(|(_, variant)| *variant)
        .ok_or_else(|| {
            let names: Vec<&str> = table.iter().map(|(name, _)| *name).collect();
            format!("\"{key}\" must be one of {}, got {value}", names.join(", "))
        })
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}
