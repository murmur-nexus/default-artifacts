//! The session's one QuickJS context, and one tool call against it.

use std::{cell::RefCell, rc::Rc};

use rquickjs::{context::EvalOptions, Context, Ctx, Runtime, Value};

use crate::{
    globals::{self, Shared},
    host::{Host, ResultStatus},
    render::{self, render},
};

/// What one call returns: the tool result's `status`, its `data` text and `truncated`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub status: ResultStatus,
    pub text: String,
    pub truncated: bool,
}

impl Outcome {
    fn error(text: String) -> Self {
        Outcome {
            status: ResultStatus::Error,
            text,
            truncated: false,
        }
    }
}

/// One QuickJS runtime and its one context, holding everything a program defines until the
/// engine is dropped. The component keeps one for the life of its instance.
pub struct Engine {
    // Declared before `runtime` so it is dropped first.
    context: Context,
    runtime: Runtime,
    shared: Rc<Shared>,
}

impl Engine {
    /// A runtime and a `Context::full` context: the language's built-ins, no module loader and
    /// no QuickJS `std` or `os` module. Every global reaches `host` for the life of the engine.
    pub fn new(host: Rc<RefCell<dyn Host>>) -> Result<Self, String> {
        let runtime =
            Runtime::new().map_err(|e| format!("tool-repl-js: no QuickJS runtime: {e}"))?;
        let context = Context::full(&runtime)
            .map_err(|e| format!("tool-repl-js: no QuickJS context: {e}"))?;
        Ok(Engine {
            context,
            runtime,
            shared: Rc::new(Shared::new(host)),
        })
    }

    /// One tool call: checks the artifact config (`MURMUR_ARTIFACT_CONFIG`'s text, `None` when
    /// unset) and the input (`tool-input.data`), then runs `code`. A bad config or input is
    /// status `error` and runs nothing.
    pub fn handle(&mut self, data: Option<&str>, config: Option<&str>) -> Outcome {
        let limit = match self.parse_config(config) {
            Ok(limit) => limit,
            Err(e) => return Outcome::error(e),
        };
        let code = match self.parse_input(data) {
            Ok(code) => code,
            Err(e) => return Outcome::error(e),
        };
        self.run(&code, limit)
    }

    /// `output_limit_bytes` from the artifact config: `None` when there is no config or the
    /// object leaves it out. The config must be a JSON object whose only key is
    /// `output_limit_bytes`, an integer from 1 to 4294967295.
    pub fn parse_config(&self, config: Option<&str>) -> Result<Option<u32>, String> {
        let Some(config) = config else {
            return Ok(None);
        };
        self.context.with(|ctx| {
            let value = parse_json(&ctx, config)
                .map_err(|e| format!("tool-repl-js: the artifact config is not JSON: {e}"))?;
            let object = match value.as_object() {
                Some(o) if !value.is_array() && !value.is_function() => o.clone(),
                _ => return Err("tool-repl-js: the artifact config must be a JSON object".into()),
            };
            let mut limit = None;
            for key in object.keys::<String>() {
                let key = key.map_err(|e| format!("tool-repl-js: the artifact config: {e}"))?;
                if key != "output_limit_bytes" {
                    return Err(format!(
                        "tool-repl-js: config key '{key}' is not known; the only key is \
                         output_limit_bytes"
                    ));
                }
                let value: Value = object
                    .get(&key)
                    .map_err(|e| format!("tool-repl-js: the artifact config: {e}"))?;
                limit = Some(
                    value
                        .as_number()
                        .filter(|n| n.fract() == 0.0 && (1.0..=4_294_967_295.0).contains(n))
                        .map(|n| n as u32)
                        .ok_or_else(|| {
                            "tool-repl-js: config output_limit_bytes must be an integer from 1 \
                             to 4294967295"
                                .to_string()
                        })?,
                );
            }
            Ok(limit)
        })
    }

    /// The `code` string of the tool input, which must be a JSON object holding one.
    pub fn parse_input(&self, data: Option<&str>) -> Result<String, String> {
        const SHAPE: &str = "the input must be a JSON object with a string `code`";
        let data = data.unwrap_or("");
        self.context.with(|ctx| {
            let value = parse_json(&ctx, data)
                .map_err(|e| format!("tool-repl-js: the input is not JSON ({e}); {SHAPE}"))?;
            let object = match value.as_object() {
                Some(o) if !value.is_array() => o.clone(),
                _ => return Err(format!("tool-repl-js: {SHAPE}")),
            };
            let code: Value = object
                .get("code")
                .map_err(|e| format!("tool-repl-js: the input: {e}"))?;
            if code.is_undefined() {
                return Err(format!("tool-repl-js: the input has no `code`; {SHAPE}"));
            }
            match code.as_string() {
                Some(s) => Ok(render::text(s)),
                None => Err(format!("tool-repl-js: `code` is not a string; {SHAPE}")),
            }
        })
    }

    /// Runs `code` as a global script in the context. Promise jobs it queues run before this returns. The text is what `print` wrote, then
    /// `=> <value>` of the last expression unless it is `undefined`, a string in JSON quotes
    /// (status `passed`); or, on a
    /// throw, `Uncaught …`, the stack, then what was printed (status `failed`). With `limit`,
    /// the text is cut to at most that many bytes and marked.
    pub fn run(&mut self, code: &str, limit: Option<u32>) -> Outcome {
        let shared = Rc::clone(&self.shared);
        shared.printed.borrow_mut().clear();
        let (status, text) = self.context.with(|ctx| {
            if let Err(e) = globals::install(&ctx, &shared) {
                ctx.catch();
                return (
                    ResultStatus::Error,
                    format!("tool-repl-js: could not install the globals: {e}"),
                );
            }
            let mut options = EvalOptions::default();
            options.global = true;
            options.strict = false;
            options.promise = false;
            let evaluated = match ctx.eval_with_options::<Value, _>(code, options) {
                Ok(value) => Ok(value),
                Err(rquickjs::Error::Exception) => Err(Thrown::Value(ctx.catch())),
                Err(other) => Err(Thrown::Rust(other.to_string())),
            };
            while ctx.execute_pending_job() {}
            let printed = take(&shared.printed);
            match evaluated {
                Ok(value) => {
                    let mut text = printed;
                    if !value.is_undefined() {
                        text.push_str("=> ");
                        text.push_str(&render::value_line(&ctx, &value));
                    }
                    (ResultStatus::Passed, text)
                }
                Err(thrown) => (ResultStatus::Failed, uncaught(&ctx, thrown) + &printed),
            }
        });
        let (text, truncated) = render::cut(text, limit);
        // Collect what this call left unreachable while the instance waits for the next one.
        self.runtime.run_gc();
        Outcome {
            status,
            text,
            truncated,
        }
    }
}

enum Thrown<'js> {
    Value(Value<'js>),
    Rust(String),
}

/// `Uncaught <name>: <message>\n` and the stack for an `Error`, `Uncaught <value>\n` otherwise.
fn uncaught<'js>(ctx: &Ctx<'js>, thrown: Thrown<'js>) -> String {
    match thrown {
        Thrown::Rust(e) => format!("Uncaught InternalError: {e}\n"),
        Thrown::Value(v) if v.is_error() => {
            let mut text = format!("Uncaught {}\n", render::error_head(ctx, &v));
            if let Some(stack) = render::property(ctx, &v, "stack") {
                if !stack.is_empty() {
                    text.push_str(&stack);
                    if !stack.ends_with('\n') {
                        text.push('\n');
                    }
                }
            }
            text
        }
        Thrown::Value(v) => format!("Uncaught {}\n", render(ctx, &v)),
    }
}

/// `JSON.parse` with QuickJS's own parser; the error is the `SyntaxError`'s message.
fn parse_json<'js>(ctx: &Ctx<'js>, text: &str) -> Result<Value<'js>, String> {
    match ctx.json_parse(text) {
        Ok(v) => Ok(v),
        Err(rquickjs::Error::Exception) => {
            let thrown = ctx.catch();
            Err(render::property(ctx, &thrown, "message").unwrap_or_else(|| render(ctx, &thrown)))
        }
        Err(e) => Err(e.to_string()),
    }
}

fn take(cell: &RefCell<String>) -> String {
    std::mem::take(&mut *cell.borrow_mut())
}
