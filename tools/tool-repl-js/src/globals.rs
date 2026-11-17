//! The globals a program calls: `tools`, `call`, `callAll`, `run` and `print`. Their names and
//! shapes are the artifact's contract.
//!
//! [`install`] sets them on `globalThis` at the start of every call, so a program that replaced
//! one gets it back on the next call. They reach the engine's [`Host`] through [`Shared`].

use std::{cell::RefCell, rc::Rc};

use rquickjs::{
    function::Rest, Array, Ctx, Exception, Function, Object, Result as JsResult, Value,
};

use crate::{
    host::{CallError, Envelope, Host, ProcError, ResultStatus, RunOutput, RunRequest},
    render::{self, render},
};

/// What the globals share with the engine.
pub(crate) struct Shared {
    /// The bus and the process runner, for the life of the engine.
    host: Rc<RefCell<dyn Host>>,
    /// What `print` wrote during this call.
    pub(crate) printed: RefCell<String>,
}

impl Shared {
    pub(crate) fn new(host: Rc<RefCell<dyn Host>>) -> Self {
        Shared {
            host,
            printed: RefCell::default(),
        }
    }
}

/// Runs `f` with the host; `None` when the host is already borrowed, which only a [`Host`] that
/// ran JavaScript from inside one of its own methods could cause.
fn with_host<R>(shared: &Shared, f: impl FnOnce(&mut dyn Host) -> R) -> Option<R> {
    let mut host = shared.host.try_borrow_mut().ok()?;
    Some(f(&mut *host))
}

/// Sets the five globals on `globalThis`.
pub(crate) fn install<'js>(ctx: &Ctx<'js>, shared: &Rc<Shared>) -> JsResult<()> {
    let globals = ctx.globals();

    let s = Rc::clone(shared);
    let print = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, values: Rest<Value<'js>>| -> JsResult<()> {
            let line = values
                .0
                .iter()
                .map(|v| render(&ctx, v))
                .collect::<Vec<_>>()
                .join(" ");
            let mut printed = s.printed.borrow_mut();
            printed.push_str(&line);
            printed.push('\n');
            Ok(())
        },
    )?;
    print.set_name("print")?;
    globals.set("print", print)?;

    let s = Rc::clone(shared);
    let tools = Function::new(ctx.clone(), move |ctx: Ctx<'js>| -> JsResult<Value<'js>> {
        let listed = with_host(&s, |h| h.list()).ok_or_else(|| host_busy(&ctx))?;
        let operations = listed.map_err(|text| ctx.throw(kind_error(&ctx, &text, "refused")))?;
        let array = Array::new(ctx.clone())?;
        for (i, op) in operations.iter().enumerate() {
            let entry = Object::new(ctx.clone())?;
            entry.set("name", op.name.as_str())?;
            entry.set("description", op.description.as_str())?;
            let schema = match ctx.json_parse(op.input_schema.as_str()) {
                Ok(v) => v,
                Err(_) => {
                    ctx.catch();
                    rquickjs::String::from_str(ctx.clone(), &op.input_schema)?.into_value()
                }
            };
            entry.set("inputSchema", schema)?;
            array.set(i, entry)?;
        }
        Ok(array.into_value())
    })?;
    tools.set_name("tools")?;
    globals.set("tools", tools)?;

    let s = Rc::clone(shared);
    let call = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, args: Rest<Value<'js>>| -> JsResult<Value<'js>> {
            let (name, input) =
                call_args(&ctx, args.0.first(), args.0.get(1), "call(name, input)")?;
            let result = with_host(&s, |h| h.call(&name, &input)).ok_or_else(|| host_busy(&ctx))?;
            call_result(&ctx, result).map_err(|thrown| ctx.throw(thrown))
        },
    )?;
    call.set_name("call")?;
    globals.set("call", call)?;

    let s = Rc::clone(shared);
    let call_all = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, args: Rest<Value<'js>>| -> JsResult<Value<'js>> {
            let pairs = call_all_args(&ctx, args.0.first())?;
            let out = Array::new(ctx.clone())?;
            if pairs.is_empty() {
                return Ok(out.into_value());
            }
            let results = with_host(&s, |h| h.call_batch(&pairs)).ok_or_else(|| host_busy(&ctx))?;
            let returned = results.len();
            let mut results = results.into_iter();
            for i in 0..pairs.len() {
                let entry = match results.next() {
                    Some(result) => call_result(&ctx, result).unwrap_or_else(|e| e),
                    None => kind_error(
                        &ctx,
                        &format!(
                            "the bus returned {returned} results for {} calls",
                            pairs.len()
                        ),
                        "failed",
                    ),
                };
                out.set(i, entry)?;
            }
            Ok(out.into_value())
        },
    )?;
    call_all.set_name("callAll")?;
    globals.set("callAll", call_all)?;

    let s = Rc::clone(shared);
    let run = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, args: Rest<Value<'js>>| -> JsResult<Value<'js>> {
            let request = run_args(&ctx, args.0.first(), args.0.get(1), args.0.get(2))?;
            let result = with_host(&s, |h| h.run(request)).ok_or_else(|| host_busy(&ctx))?;
            match result {
                Ok(output) => run_output(&ctx, &output),
                Err(e) => Err(ctx.throw(process_error(&ctx, &e))),
            }
        },
    )?;
    run.set_name("run")?;
    globals.set("run", run)?;

    Ok(())
}

/// The error a global throws when the host is already answering a call.
fn host_busy(ctx: &Ctx<'_>) -> rquickjs::Error {
    Exception::throw_message(ctx, "tool-repl-js: the host is already answering a call")
}

/// A new `Error` with `message` and `kind`, as a value (not thrown).
fn kind_error<'js>(ctx: &Ctx<'js>, message: &str, kind: &str) -> Value<'js> {
    let made = Exception::from_message(ctx.clone(), message).and_then(|e| {
        let object = e.into_object();
        object.set("kind", kind)?;
        Ok(object.into_value())
    });
    made.unwrap_or_else(|_| {
        ctx.catch();
        Value::new_undefined(ctx.clone())
    })
}

fn type_error(ctx: &Ctx<'_>, message: &str) -> rquickjs::Error {
    Exception::throw_type(ctx, message)
}

/// `name` and `input` of one call as the bus takes them: the name, and `JSON.stringify(input)`
/// with `{}` for a missing or `undefined` input. A `TypeError` names what is wrong.
fn call_args<'js>(
    ctx: &Ctx<'js>,
    name: Option<&Value<'js>>,
    input: Option<&Value<'js>>,
    what: &str,
) -> JsResult<(String, String)> {
    let name = match name.and_then(|n| n.as_string()) {
        Some(s) => render::text(s),
        None => return Err(type_error(ctx, &format!("{what}: name must be a string"))),
    };
    let input = match input {
        None => "{}".to_string(),
        Some(v) if v.is_undefined() => "{}".to_string(),
        Some(v) => match ctx.json_stringify(v.clone()) {
            Ok(Some(s)) => render::text(&s),
            Ok(None) => {
                return Err(type_error(
                    ctx,
                    &format!("{what}: input does not serialize to JSON"),
                ))
            }
            Err(_) => {
                let thrown = ctx.catch();
                let why = render::property(ctx, &thrown, "message")
                    .unwrap_or_else(|| render(ctx, &thrown));
                return Err(type_error(
                    ctx,
                    &format!("{what}: input does not serialize to JSON: {why}"),
                ));
            }
        },
    };
    Ok((name, input))
}

/// `pairs` of `callAll`: an array of `[name, input?]` arrays.
fn call_all_args<'js>(
    ctx: &Ctx<'js>,
    pairs: Option<&Value<'js>>,
) -> JsResult<Vec<(String, String)>> {
    const SHAPE: &str = "callAll(pairs): pairs must be an array of [name, input] arrays";
    let Some(array) = pairs.and_then(|p| p.as_array()) else {
        return Err(type_error(ctx, SHAPE));
    };
    let mut out = Vec::with_capacity(array.len());
    for i in 0..array.len() {
        let pair: Value = array.get(i)?;
        let Some(pair) = pair.as_array() else {
            return Err(type_error(
                ctx,
                &format!("{SHAPE}; pairs[{i}] is not an array"),
            ));
        };
        if pair.is_empty() || pair.len() > 2 {
            return Err(type_error(
                ctx,
                &format!("{SHAPE}; pairs[{i}] has {} elements", pair.len()),
            ));
        }
        let name: Value = pair.get(0)?;
        let input: Option<Value> = if pair.len() == 2 {
            Some(pair.get(1)?)
        } else {
            None
        };
        out.push(call_args(
            ctx,
            Some(&name),
            input.as_ref(),
            &format!("callAll(pairs): pairs[{i}]"),
        )?);
    }
    Ok(out)
}

/// The value `call` returns, or the `Error` it throws.
fn call_result<'js>(
    ctx: &Ctx<'js>,
    result: Result<Envelope, CallError>,
) -> Result<Value<'js>, Value<'js>> {
    match result {
        Err(CallError::Refused(text)) => Err(kind_error(ctx, &text, "refused")),
        Err(CallError::Failed(text)) => Err(kind_error(ctx, &text, "failed")),
        Ok(envelope) => {
            let value = envelope_value(ctx, &envelope).map_err(|_| {
                ctx.catch();
                kind_error(ctx, "tool-repl-js: could not build the result", "failed")
            })?;
            if envelope.status != ResultStatus::Error {
                return Ok(value);
            }
            let message = envelope
                .data
                .as_deref()
                .or(envelope.summary.as_deref())
                .unwrap_or("tool returned status error");
            let error = kind_error(ctx, message, "error");
            if let Some(object) = error.as_object() {
                let _ = object.set("result", value);
            }
            Err(error)
        }
    }
}

/// `{status, summary, data, dataPath, truncated, metadata, attachments}`.
fn envelope_value<'js>(ctx: &Ctx<'js>, envelope: &Envelope) -> JsResult<Value<'js>> {
    let object = Object::new(ctx.clone())?;
    object.set(
        "status",
        match envelope.status {
            ResultStatus::Passed => "passed",
            ResultStatus::Failed => "failed",
            ResultStatus::Error => "error",
        },
    )?;
    object.set("summary", text_or_null(ctx, envelope.summary.as_deref())?)?;
    object.set("data", text_or_null(ctx, envelope.data.as_deref())?)?;
    object.set(
        "dataPath",
        text_or_null(ctx, envelope.data_path.as_deref())?,
    )?;
    object.set("truncated", envelope.truncated)?;
    let metadata = Object::new(ctx.clone())?;
    for (key, value) in &envelope.metadata {
        if !metadata.contains_key(key.as_str())? {
            metadata.set(key.as_str(), value.as_str())?;
        }
    }
    object.set("metadata", metadata)?;
    let attachments = Array::new(ctx.clone())?;
    for (i, a) in envelope.attachments.iter().enumerate() {
        let entry = Object::new(ctx.clone())?;
        entry.set("mediaType", a.media_type.as_str())?;
        entry.set("path", a.path.as_str())?;
        attachments.set(i, entry)?;
    }
    object.set("attachments", attachments)?;
    Ok(object.into_value())
}

/// A string, or `null` when absent.
fn text_or_null<'js>(ctx: &Ctx<'js>, text: Option<&str>) -> JsResult<Value<'js>> {
    match text {
        Some(t) => Ok(rquickjs::String::from_str(ctx.clone(), t)?.into_value()),
        None => Ok(Value::new_null(ctx.clone())),
    }
}

/// `run(program, args, options)` as a [`RunRequest`].
fn run_args<'js>(
    ctx: &Ctx<'js>,
    program: Option<&Value<'js>>,
    args: Option<&Value<'js>>,
    options: Option<&Value<'js>>,
) -> JsResult<RunRequest> {
    const WHAT: &str = "run(program, args, options)";
    let program = match program.and_then(|p| p.as_string()) {
        Some(s) => render::text(s),
        None => {
            return Err(type_error(
                ctx,
                &format!("{WHAT}: program must be a string"),
            ))
        }
    };
    let mut request = RunRequest {
        program,
        ..RunRequest::default()
    };
    match args {
        None => {}
        Some(v) if v.is_undefined() => {}
        Some(v) => {
            let array = v.as_array().ok_or_else(|| {
                type_error(ctx, &format!("{WHAT}: args must be an array of strings"))
            })?;
            for i in 0..array.len() {
                let arg: Value = array.get(i)?;
                match arg.as_string() {
                    Some(s) => request.args.push(render::text(s)),
                    None => {
                        return Err(type_error(
                            ctx,
                            &format!("{WHAT}: args must be an array of strings; args[{i}] is not"),
                        ))
                    }
                }
            }
        }
    }
    let options = match options {
        None => return Ok(request),
        Some(v) if v.is_undefined() => return Ok(request),
        Some(v) => match v.as_object() {
            Some(o) if !v.is_array() && !v.is_function() => o.clone(),
            _ => {
                return Err(type_error(
                    ctx,
                    &format!("{WHAT}: options must be an object holding env, stdin or timeoutMs"),
                ))
            }
        },
    };
    for key in options.keys::<String>() {
        let key = key?;
        let value: Value = options.get(key.as_str())?;
        match key.as_str() {
            "env" => {
                let env = match value.as_object() {
                    Some(o) if !value.is_array() && !value.is_function() => o.clone(),
                    _ => {
                        return Err(type_error(
                            ctx,
                            &format!("{WHAT}: options.env must be an object of string values"),
                        ))
                    }
                };
                for name in env.keys::<String>() {
                    let name = name?;
                    let v: Value = env.get(name.as_str())?;
                    match v.as_string() {
                        Some(s) => request.env.push((name, render::text(s))),
                        None => {
                            return Err(type_error(
                                ctx,
                                &format!(
                                    "{WHAT}: options.env must be an object of string values; \
                                     env.{name} is not a string"
                                ),
                            ))
                        }
                    }
                }
            }
            "stdin" => match value.as_string() {
                Some(s) => request.stdin = Some(render::text(s).into_bytes()),
                None => {
                    return Err(type_error(
                        ctx,
                        &format!("{WHAT}: options.stdin must be a string"),
                    ))
                }
            },
            "timeoutMs" => {
                let ms = value
                    .as_number()
                    .filter(|n| n.fract() == 0.0 && (0.0..=9_007_199_254_740_991.0).contains(n))
                    .ok_or_else(|| {
                        type_error(
                            ctx,
                            &format!("{WHAT}: options.timeoutMs must be a non-negative integer"),
                        )
                    })?;
                request.timeout_ms = Some(ms as u64);
            }
            other => {
                return Err(type_error(
                    ctx,
                    &format!(
                        "{WHAT}: options may hold only env, stdin and timeoutMs; '{other}' is \
                         not one of them"
                    ),
                ))
            }
        }
    }
    Ok(request)
}

/// `{exitCode, timedOut, stdout, stdoutTruncated, stderr, stderrTruncated}`.
fn run_output<'js>(ctx: &Ctx<'js>, output: &RunOutput) -> JsResult<Value<'js>> {
    let object = Object::new(ctx.clone())?;
    object.set("exitCode", output.exit_code)?;
    object.set("timedOut", output.timed_out)?;
    object.set("stdout", String::from_utf8_lossy(&output.stdout).as_ref())?;
    object.set("stdoutTruncated", output.stdout_truncated)?;
    object.set("stderr", String::from_utf8_lossy(&output.stderr).as_ref())?;
    object.set("stderrTruncated", output.stderr_truncated)?;
    Ok(object.into_value())
}

/// The `Error` `run` throws for a `process-error`.
fn process_error<'js>(ctx: &Ctx<'js>, e: &ProcError) -> Value<'js> {
    match e {
        ProcError::Refused { rule, reason } => {
            let error = kind_error(ctx, &format!("{rule}: {reason}"), "refused");
            if let Some(object) = error.as_object() {
                let _ = object.set("rule", rule.as_str());
            }
            error
        }
        ProcError::UnknownHandle => kind_error(ctx, "unknown handle", "unknown-handle"),
        ProcError::NotRunning => kind_error(ctx, "the process is not running", "not-running"),
        ProcError::TimedOut => kind_error(ctx, "the program timed out", "timed-out"),
        ProcError::Failed(text) => kind_error(ctx, text, "failed"),
    }
}
