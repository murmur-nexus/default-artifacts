//! How a value reads in the tool's text, and the output cut.

use rquickjs::{convert::Coerced, Ctx, Value};

/// A value as `print` writes it, and as the `=> ` line shows it but for a string
/// ([`value_line`]):
///
/// | Value | Text |
/// |---|---|
/// | string | as is |
/// | `undefined` | `undefined` |
/// | number, boolean, `null` | what `JSON.stringify` writes |
/// | bigint | `<digits>n` |
/// | symbol | `String(v)` |
/// | function | `[function <name>]`, or `[function]` without a name |
/// | `Error` | `<name>: <message>` |
/// | any other object or array | compact `JSON.stringify(v)`, else `String(v)` |
///
/// `JSON.stringify` is QuickJS's own, so a program that replaces `globalThis.JSON` does not
/// change what this writes.
pub(crate) fn render<'js>(ctx: &Ctx<'js>, v: &Value<'js>) -> String {
    if v.is_undefined() {
        "undefined".into()
    } else if let Some(s) = v.as_string() {
        text(s)
    } else if v.is_null() {
        "null".into()
    } else if v.is_bool() || v.is_number() {
        json(ctx, v).unwrap_or_else(|| string_of(ctx, v))
    } else if v.is_big_int() {
        format!("{}n", string_of(ctx, v))
    } else if let Some(symbol) = v.as_symbol() {
        match symbol.description() {
            Ok(d) if !d.is_undefined() => format!("Symbol({})", string_of(ctx, &d)),
            _ => "Symbol()".into(),
        }
    } else if v.is_function() {
        match property(ctx, v, "name") {
            Some(name) if !name.is_empty() => format!("[function {name}]"),
            _ => "[function]".into(),
        }
    } else if v.is_error() {
        error_head(ctx, v)
    } else {
        json(ctx, v).unwrap_or_else(|| string_of(ctx, v))
    }
}

/// The last expression's value as the `=> ` line shows it: as [`render`] writes it, but a string
/// in JSON quotes, so the line stays one line and `"42"` reads apart from `42`.
pub(crate) fn value_line<'js>(ctx: &Ctx<'js>, v: &Value<'js>) -> String {
    if v.is_string() {
        if let Some(quoted) = json(ctx, v) {
            return quoted;
        }
    }
    render(ctx, v)
}

/// `<name>: <message>` of an `Error`.
pub(crate) fn error_head<'js>(ctx: &Ctx<'js>, v: &Value<'js>) -> String {
    let name = property(ctx, v, "name").unwrap_or_else(|| "Error".into());
    let message = property(ctx, v, "message").unwrap_or_default();
    format!("{name}: {message}")
}

/// `v[key]` as a string, `None` when it is `undefined` or reading it throws.
pub(crate) fn property<'js>(ctx: &Ctx<'js>, v: &Value<'js>, key: &str) -> Option<String> {
    let object = v.as_object()?;
    match object.get::<_, Value>(key) {
        Ok(value) if value.is_undefined() => None,
        Ok(value) => Some(string_of(ctx, &value)),
        Err(_) => {
            ctx.catch();
            None
        }
    }
}

/// A JavaScript string as Rust text; a lone surrogate becomes U+FFFD.
pub(crate) fn text(s: &rquickjs::String<'_>) -> String {
    s.to_string().unwrap_or_else(|_| {
        s.clone()
            .to_cstring()
            .map(|c| c.as_str().to_owned())
            .unwrap_or_else(|_| "\u{FFFD}".into())
    })
}

/// Compact `JSON.stringify(v)`, `None` when it throws or returns `undefined`.
pub(crate) fn json<'js>(ctx: &Ctx<'js>, v: &Value<'js>) -> Option<String> {
    match ctx.json_stringify(v.clone()) {
        Ok(Some(s)) => Some(text(&s)),
        Ok(None) => None,
        Err(_) => {
            ctx.catch();
            None
        }
    }
}

/// `String(v)`; `[object]` when that throws.
fn string_of<'js>(ctx: &Ctx<'js>, v: &Value<'js>) -> String {
    if let Some(s) = v.as_string() {
        return text(s);
    }
    match v.get::<Coerced<String>>() {
        Ok(Coerced(s)) => s,
        Err(_) => {
            ctx.catch();
            "[object]".into()
        }
    }
}

/// Cuts `text` past `limit` bytes on a UTF-8 boundary and appends the mark. The mark does not
/// count toward the limit. Returns the text and whether it was cut.
pub fn cut(text: String, limit: Option<u32>) -> (String, bool) {
    let Some(limit) = limit else {
        return (text, false);
    };
    let total = text.len();
    let mut kept = usize::try_from(limit).unwrap_or(usize::MAX).min(total);
    if kept == total {
        return (text, false);
    }
    while !text.is_char_boundary(kept) {
        kept -= 1;
    }
    let mut out = text;
    out.truncate(kept);
    out.push_str(&format!(
        "\n[output cut: {total} bytes, the first {kept} shown]"
    ));
    (out, true)
}
