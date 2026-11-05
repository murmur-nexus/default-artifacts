//! The tool's input: the `data` object of the call, and the checks that refuse a call before
//! anything is started.

use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value};

/// Every action, in the order the manifest's `action` enum lists them.
pub const ACTIONS: [&str; 12] = [
    "open",
    "snapshot",
    "click",
    "type",
    "press",
    "select",
    "wait",
    "back",
    "console",
    "network",
    "screenshot",
    "close",
];

/// The longest `wait` a call may ask for, and the default when `ms` is absent.
pub const WAIT_MAX_MS: u64 = 60_000;
pub const WAIT_DEFAULT_MS: u64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Open,
    Snapshot,
    Click,
    Type,
    Press,
    Select,
    Wait,
    Back,
    Console,
    Network,
    Screenshot,
    Close,
}

impl Action {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "open" => Self::Open,
            "snapshot" => Self::Snapshot,
            "click" => Self::Click,
            "type" => Self::Type,
            "press" => Self::Press,
            "select" => Self::Select,
            "wait" => Self::Wait,
            "back" => Self::Back,
            "console" => Self::Console,
            "network" => Self::Network,
            "screenshot" => Self::Screenshot,
            "close" => Self::Close,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Snapshot => "snapshot",
            Self::Click => "click",
            Self::Type => "type",
            Self::Press => "press",
            Self::Select => "select",
            Self::Wait => "wait",
            Self::Back => "back",
            Self::Console => "console",
            Self::Network => "network",
            Self::Screenshot => "screenshot",
            Self::Close => "close",
        }
    }
}

/// One validated call. Fields an action does not use are kept but ignored.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub action: Action,
    pub url: Option<String>,
    pub r#ref: Option<u32>,
    pub text: Option<String>,
    pub key: Option<KeySpec>,
    pub value: Option<String>,
    pub submit: bool,
    pub full_page: bool,
    pub ms: Option<u64>,
}

/// The call's `data`: a JSON object, or a JSON-encoded string of one.
pub fn params_from_data(raw: &str) -> Result<Value, String> {
    if raw.trim().is_empty() {
        return Err("tool-browser: missing input".into());
    }
    let data: Value =
        serde_json::from_str(raw).map_err(|e| format!("tool-browser: invalid input JSON: {e}"))?;
    params_from_value(data)
}

fn params_from_value(data: Value) -> Result<Value, String> {
    match data {
        Value::String(s) => serde_json::from_str(&s)
            .map_err(|e| format!("tool-browser: invalid data JSON string: {e}")),
        Value::Object(_) => Ok(data),
        _ => Err("tool-browser: data must be a JSON object or a JSON-encoded string".into()),
    }
}

/// A string field, `None` when absent, `null` or empty. A model filling every schema property
/// sends `null` for the ones it does not mean.
fn string_field(
    obj: &Map<String, Value>,
    field: &str,
    action: &str,
) -> Result<Option<String>, String> {
    match obj.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("{field} must be a string (action {action})")),
    }
}

fn bool_field(obj: &Map<String, Value>, field: &str, action: &str) -> Result<bool, String> {
    match obj.get(field) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("{field} must be a boolean (action {action})")),
    }
}

fn ms_field(obj: &Map<String, Value>, action: &str) -> Result<Option<u64>, String> {
    match obj.get("ms") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => {
            let ms = n
                .as_u64()
                .or_else(|| {
                    n.as_f64()
                        .filter(|f| f.fract() == 0.0 && *f >= 0.0)
                        .map(|f| f as u64)
                })
                .ok_or_else(|| {
                    format!(
                        "ms must be a whole number of milliseconds, 0 or more (action {action})"
                    )
                })?;
            if ms > WAIT_MAX_MS {
                return Err(format!(
                    "ms is {ms}, but {action} waits at most {WAIT_MAX_MS} ms"
                ));
            }
            Ok(Some(ms))
        }
        Some(_) => Err(format!("ms must be an integer (action {action})")),
    }
}

/// `e12` → 12.
pub fn parse_ref(text: &str) -> Option<u32> {
    let digits = text.trim().strip_prefix('e')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|n| *n > 0)
}

/// Validate `params` into a [`Request`], refusing with one line naming the field and action.
pub fn parse_request(params: &Value) -> Result<Request, String> {
    let obj = params
        .as_object()
        .ok_or("tool-browser: data must be a JSON object with an action")?;
    let action_name = match obj.get("action") {
        None | Some(Value::Null) => {
            return Err(format!("missing action; use one of {}", ACTIONS.join(", ")))
        }
        Some(Value::String(s)) => s.as_str(),
        Some(_) => return Err("action must be a string".into()),
    };
    let action = Action::parse(action_name).ok_or_else(|| {
        format!(
            "unknown action \"{}\"; use one of {}",
            crate::util::one_line(action_name, 60),
            ACTIONS.join(", ")
        )
    })?;
    let a = action.name();

    let url = string_field(obj, "url", a)?;
    let ref_text = string_field(obj, "ref", a)?;
    let text = string_field(obj, "text", a)?;
    let key_text = string_field(obj, "key", a)?;
    let value = string_field(obj, "value", a)?;
    let submit = bool_field(obj, "submit", a)?;
    let full_page = bool_field(obj, "full_page", a)?;
    let ms = ms_field(obj, a)?;

    let r#ref = match &ref_text {
        None => None,
        Some(t) => Some(parse_ref(t).ok_or_else(|| {
            format!(
                "ref \"{}\" is not a ref; pass one such as e3 from a snapshot (action {a})",
                crate::util::one_line(t, 40)
            )
        })?),
    };

    let need = |present: bool, field: &str, what: &str| -> Result<(), String> {
        if present {
            Ok(())
        } else {
            Err(format!("{a} needs {field} ({what})"))
        }
    };
    match action {
        Action::Open => need(
            url.is_some(),
            "url",
            "an http, https or file URL, about:blank, or a path in the work folder",
        )?,
        Action::Click | Action::Select => {
            need(r#ref.is_some(), "ref", "a ref such as e3 from a snapshot")?
        }
        Action::Type => {
            need(r#ref.is_some(), "ref", "a ref such as e3 from a snapshot")?;
            need(text.is_some(), "text", "the text to insert")?;
        }
        Action::Press => need(
            key_text.is_some(),
            "key",
            "a key such as Enter, Tab or Control+a",
        )?,
        Action::Wait if text.is_none() && r#ref.is_none() && ms.is_none() => {
            return Err("wait needs one of text, ref or ms".into());
        }
        _ => {}
    }
    if action == Action::Select {
        need(
            value.is_some(),
            "value",
            "the option's value or visible label",
        )?;
    }
    let key = match (&key_text, action) {
        (Some(k), Action::Press) => Some(parse_key(k)?),
        _ => None,
    };

    Ok(Request {
        action,
        url,
        r#ref,
        text,
        key,
        value,
        submit,
        full_page,
        ms,
    })
}

// ── keys ─────────────────────────────────────────────────────────────────────

/// A key for `Input.dispatchKeyEvent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeySpec {
    /// The DOM `key` value: `Enter`, `a`, `A`.
    pub key: String,
    /// The DOM `code` value: `Enter`, `KeyA`; empty when there is none.
    pub code: String,
    /// The Windows virtual key code Chrome expects in `windowsVirtualKeyCode`.
    pub key_code: u32,
    /// The text the key inserts, when it inserts any and no command modifier is held.
    pub text: Option<String>,
    /// CDP modifier bits: Alt 1, Control 2, Meta 4, Shift 8.
    pub modifiers: u32,
}

const NAMED_KEYS: [(&str, u32, Option<&str>); 15] = [
    ("Enter", 13, Some("\r")),
    ("Tab", 9, None),
    ("Escape", 27, None),
    ("Backspace", 8, None),
    ("Delete", 46, None),
    ("Space", 32, Some(" ")),
    ("ArrowUp", 38, None),
    ("ArrowDown", 40, None),
    ("ArrowLeft", 37, None),
    ("ArrowRight", 39, None),
    ("Home", 36, None),
    ("End", 35, None),
    ("PageUp", 33, None),
    ("PageDown", 34, None),
    ("Esc", 27, None),
];

fn unknown_modifier(modifier: &str, spec: &str) -> String {
    format!(
        "press: unknown modifier \"{}\" in key \"{}\"; use Control+, Shift+, Alt+ or Meta+",
        crate::util::one_line(modifier, 30),
        crate::util::one_line(spec, 40)
    )
}

/// `Enter`, `a`, `Shift+Tab`, `Control+Enter`.
pub fn parse_key(spec: &str) -> Result<KeySpec, String> {
    let mut modifiers = 0;
    let mut rest = spec;
    // `Control++` or a bare `+` ends with an empty tail: the `+` is the key.
    while let Some((prefix, tail)) = rest.split_once('+').filter(|(_, t)| !t.is_empty()) {
        modifiers |= match prefix {
            "Alt" => 1,
            "Control" | "Ctrl" => 2,
            "Meta" | "Cmd" => 4,
            "Shift" => 8,
            other => return Err(unknown_modifier(other, spec)),
        };
        rest = tail;
    }
    let command_held = modifiers & (1 | 2 | 4) != 0;
    if let Some((name, code, text)) = NAMED_KEYS.iter().find(|(n, _, _)| *n == rest) {
        let (key, dom_code) = match *name {
            "Space" => (" ", "Space"),
            "Esc" => ("Escape", "Escape"),
            n => (n, n),
        };
        return Ok(KeySpec {
            key: key.to_string(),
            code: dom_code.to_string(),
            key_code: *code,
            text: text.filter(|_| !command_held).map(str::to_string),
            modifiers,
        });
    }
    let mut chars = rest.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if !c.is_control() => {
            let (code, key_code) = match c {
                'a'..='z' | 'A'..='Z' => (
                    format!("Key{}", c.to_ascii_uppercase()),
                    c.to_ascii_uppercase() as u32,
                ),
                '0'..='9' => (format!("Digit{c}"), c as u32),
                _ => (String::new(), 0),
            };
            let shown = if modifiers & 8 != 0 && c.is_ascii_lowercase() {
                c.to_ascii_uppercase()
            } else {
                c
            };
            Ok(KeySpec {
                key: shown.to_string(),
                code,
                key_code,
                text: (!command_held).then(|| shown.to_string()),
                modifiers,
            })
        }
        _ => Err(format!(
            "press: unknown key \"{}\"; use Enter, Tab, Escape, Backspace, Delete, Space, ArrowUp, ArrowDown, ArrowLeft, ArrowRight, Home, End, PageUp, PageDown or one character, optionally after Control+, Shift+, Alt+ or Meta+",
            crate::util::one_line(spec, 40)
        )),
    }
}

// ── open targets ─────────────────────────────────────────────────────────────

/// What `open` navigates to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenTarget {
    /// An `http`, `https` or `file` URL, or `about:blank`, passed on as written.
    Url(String),
    /// A file inside the work folder, relative to it.
    WorkFile(PathBuf),
}

/// The scheme of `text` when it starts with one (`http:`), lowercased.
fn scheme_of(text: &str) -> Option<String> {
    let colon = text.find(':')?;
    let scheme = &text[..colon];
    let mut bytes = scheme.bytes();
    let first = bytes.next()?;
    (first.is_ascii_alphabetic()
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.'))
    .then(|| scheme.to_ascii_lowercase())
}

/// Classify `url` for `open`, refusing the schemes it never navigates to and any path that is
/// not an existing file inside the work folder.
///
/// `workdir` is the work folder as the tool opens it (`.` in the component, whose WASI preopen
/// refuses every path out of it, a link included); `workdir_abs` is its host path, once Chrome has
/// shown it, against which an absolute path is checked. With no `workdir_abs`, an absolute path
/// is refused as outside the work folder.
pub fn open_target(
    url: &str,
    workdir: &Path,
    workdir_abs: Option<&Path>,
) -> Result<OpenTarget, String> {
    let url = url.trim();
    match scheme_of(url).as_deref() {
        Some("http" | "https" | "file") => return Ok(OpenTarget::Url(url.to_string())),
        Some("about") if url.eq_ignore_ascii_case("about:blank") => {
            return Ok(OpenTarget::Url("about:blank".into()))
        }
        Some(s @ ("javascript" | "data" | "chrome")) => {
            return Err(format!(
                "open refuses {s}: URLs; pass an http, https or file URL, about:blank, or a path in the work folder"
            ))
        }
        Some(s) => {
            return Err(format!(
                "open does not navigate to {s}: URLs; pass an http, https or file URL, about:blank, or a path in the work folder"
            ))
        }
        None => {}
    }
    let given = Path::new(url);
    let relative: PathBuf = if given.is_absolute() {
        match workdir_abs.and_then(|root| given.strip_prefix(root).ok()) {
            Some(rest) => rest.to_path_buf(),
            None => {
                return Err(format!(
                    "open refuses {url}: the path is outside the work folder"
                ))
            }
        }
    } else {
        given.to_path_buf()
    };
    // Resolved lexically: a `..` that climbs above the work folder is refused by name.
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    for c in relative.components() {
        match c {
            Component::ParentDir if parts.pop().is_none() => {
                return Err(format!(
                    "open refuses {url}: the path climbs out of the work folder"
                ));
            }
            Component::Normal(p) => parts.push(p),
            _ => {}
        }
    }
    let normal: PathBuf = parts.iter().collect();
    if normal.as_os_str().is_empty() {
        return Err(format!("open: {url} is not a file in the work folder"));
    }
    match workdir.join(&normal).metadata() {
        Err(_) => Err(format!(
            "open: {url} is not a URL and no such file is in the work folder"
        )),
        Ok(m) if !m.is_file() => Err(format!("open: {url} is not a file in the work folder")),
        Ok(_) => Ok(OpenTarget::WorkFile(normal)),
    }
}

/// `file://` URL for an absolute path, percent-encoding every byte outside the unreserved set.
pub fn file_url(path: &Path) -> String {
    let mut out = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn err(v: Value) -> String {
        parse_request(&v).unwrap_err()
    }

    #[test]
    fn data_takes_object_or_encoded_string() {
        let a = json!({"action": "snapshot"});
        assert_eq!(params_from_data(r#"{"action":"snapshot"}"#).unwrap(), a);
        assert_eq!(
            params_from_data(r#""{\"action\":\"snapshot\"}""#).unwrap(),
            a
        );
        assert!(params_from_data("3").is_err());
        assert!(params_from_data(" ").is_err());
    }

    #[test]
    fn refuses_unknown_action_and_missing_fields() {
        assert!(err(json!({"action":"fly"})).starts_with("unknown action \"fly\""));
        assert_eq!(
            err(json!({"action":"click"})),
            "click needs ref (a ref such as e3 from a snapshot)"
        );
        assert!(err(json!({"action":"open"})).starts_with("open needs url"));
        assert!(err(json!({"action":"type","ref":"e1"})).starts_with("type needs text"));
        assert!(err(json!({"action":"press"})).starts_with("press needs key"));
        assert!(err(json!({"action":"select","ref":"e1"})).starts_with("select needs value"));
        assert_eq!(
            err(json!({"action":"wait"})),
            "wait needs one of text, ref or ms"
        );
    }

    #[test]
    fn refuses_wrong_types_naming_field_and_action() {
        assert_eq!(
            err(json!({"action":"click","ref":3})),
            "ref must be a string (action click)"
        );
        assert_eq!(
            err(json!({"action":"type","ref":"e1","text":"x","submit":"yes"})),
            "submit must be a boolean (action type)"
        );
        assert_eq!(
            err(json!({"action":"wait","ms":"10"})),
            "ms must be an integer (action wait)"
        );
        assert!(err(json!({"action":"wait","ms":70000})).contains("at most 60000"));
        assert!(err(json!({"action":"click","ref":"button"})).contains("is not a ref"));
    }

    #[test]
    fn nulls_count_as_absent() {
        let r = parse_request(
            &json!({"action":"snapshot","url":null,"ref":null,"ms":null,"submit":null}),
        )
        .unwrap();
        assert_eq!(r.action, Action::Snapshot);
        assert_eq!(r.r#ref, None);
    }

    #[test]
    fn keys_parse() {
        let k = parse_key("Enter").unwrap();
        assert_eq!((k.key_code, k.text.as_deref()), (13, Some("\r")));
        let k = parse_key("Control+a").unwrap();
        assert_eq!(
            (k.modifiers, k.text.as_deref(), k.code.as_str()),
            (2, None, "KeyA")
        );
        let k = parse_key("Shift+a").unwrap();
        assert_eq!(k.text.as_deref(), Some("A"));
        assert!(parse_key("Hyper+a").is_err());
        assert!(parse_key("F13").is_err());
        assert_eq!(parse_key("+").unwrap().key, "+");
    }

    #[test]
    fn open_targets() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("page.html"), "<p>x</p>").unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let t = |u: &str| open_target(u, root, Some(root));
        assert_eq!(t("page.html"), Ok(OpenTarget::WorkFile("page.html".into())));
        assert_eq!(
            t("./sub/../page.html"),
            Ok(OpenTarget::WorkFile("page.html".into()))
        );
        assert_eq!(
            t(&root.join("page.html").display().to_string()),
            Ok(OpenTarget::WorkFile("page.html".into()))
        );
        assert!(matches!(t("https://example.com/"), Ok(OpenTarget::Url(_))));
        assert!(t("javascript:alert(1)")
            .unwrap_err()
            .contains("javascript:"));
        assert!(t("data:text/html,x").unwrap_err().contains("data:"));
        assert!(t("chrome://version").unwrap_err().contains("chrome:"));
        assert!(t("../outside.html").unwrap_err().contains("climbs out"));
        assert!(t("sub/../../outside.html")
            .unwrap_err()
            .contains("climbs out"));
        assert!(t("/etc/hostname")
            .unwrap_err()
            .contains("outside the work folder"));
        assert!(open_target("/etc/hostname", root, None)
            .unwrap_err()
            .contains("outside the work folder"));
        assert!(t("nope.html").unwrap_err().contains("no such file"));
        assert!(t("sub").unwrap_err().contains("is not a file"));
    }

    #[test]
    fn file_urls_are_encoded() {
        assert_eq!(
            file_url(Path::new("/a b/c#d.html")),
            "file:///a%20b/c%23d.html"
        );
    }
}
