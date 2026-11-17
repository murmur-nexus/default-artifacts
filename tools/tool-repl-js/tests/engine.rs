//! The engine against a fake [`Host`]: state across calls, rendering, the output formats, the
//! cut, the config and input checks, and every global's happy path, `TypeError` and `Error.kind`.

use std::{cell::RefCell, rc::Rc};

use tool_repl_js::{
    Attachment, CallError, Engine, Envelope, Host, Operation, Outcome, ProcError, ResultStatus,
    RunOutput, RunRequest,
};

/// A host that answers from fields and records what it was asked.
#[derive(Default)]
struct Fake {
    list: Option<Result<Vec<Operation>, String>>,
    /// Answers by name; a name with no answer is refused the way the bus refuses it.
    answers: Vec<(String, Result<Envelope, CallError>)>,
    run_answer: Option<Result<RunOutput, ProcError>>,
    calls: Vec<(String, String)>,
    batches: Vec<Vec<(String, String)>>,
    runs: Vec<RunRequest>,
    lists: usize,
}

fn refusal(name: &str) -> CallError {
    CallError::Refused(format!(
        "'{name}' is not an operation this program may call; list() returns every operation it may call"
    ))
}

impl Fake {
    fn answer(&self, name: &str) -> Result<Envelope, CallError> {
        self.answers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, a)| a.clone())
            .unwrap_or_else(|| Err(refusal(name)))
    }
}

impl Host for Fake {
    fn list(&mut self) -> Result<Vec<Operation>, String> {
        self.lists += 1;
        self.list.clone().unwrap_or_else(|| Ok(Vec::new()))
    }
    fn call(&mut self, name: &str, input: &str) -> Result<Envelope, CallError> {
        self.calls.push((name.into(), input.into()));
        self.answer(name)
    }
    fn call_batch(&mut self, calls: &[(String, String)]) -> Vec<Result<Envelope, CallError>> {
        self.batches.push(calls.to_vec());
        calls.iter().map(|(n, _)| self.answer(n)).collect()
    }
    fn run(&mut self, request: RunRequest) -> Result<RunOutput, ProcError> {
        self.runs.push(request);
        self.run_answer
            .clone()
            .unwrap_or_else(|| Ok(RunOutput::default()))
    }
}

fn passed(data: &str) -> Envelope {
    Envelope {
        status: ResultStatus::Passed,
        summary: None,
        data: Some(data.into()),
        data_path: None,
        truncated: false,
        metadata: Vec::new(),
        attachments: Vec::new(),
    }
}

fn eval(engine: &mut Engine, code: &str) -> Outcome {
    engine.run(code, None)
}

/// The text of a call that must pass.
fn ok(engine: &mut Engine, code: &str) -> String {
    let out = eval(engine, code);
    assert_eq!(out.status, ResultStatus::Passed, "{code}: {}", out.text);
    assert!(!out.truncated);
    out.text
}

/// The text of a call that must fail.
fn failed(engine: &mut Engine, code: &str) -> String {
    let out = eval(engine, code);
    assert_eq!(out.status, ResultStatus::Failed, "{code}: {}", out.text);
    out.text
}

/// An engine and the fake it reaches, which the test keeps to set answers and read records.
fn fresh() -> (Engine, Rc<RefCell<Fake>>) {
    let host = Rc::new(RefCell::new(Fake::default()));
    (Engine::new(host.clone()).unwrap(), host)
}

// ── state ────────────────────────────────────────────────────────────────────

#[test]
fn state_lasts_across_calls() {
    let (mut e, _host) = fresh();
    assert_eq!(
        ok(&mut e, "globalThis.n = 41; let kept = 'yes'; var v = 1; function f() { return 2 } class C {}; print('set')"),
        "set\n"
    );
    assert_eq!(
        ok(&mut e, "[n + 1, kept, v, f(), typeof C]"),
        "=> [42,\"yes\",1,2,\"function\"]"
    );
}

#[test]
fn a_throw_keeps_the_context() {
    let (mut e, _host) = fresh();
    ok(&mut e, "var a = 1");
    failed(&mut e, "a = 2; throw new Error('x')");
    assert_eq!(ok(&mut e, "a"), "=> 2");
}

#[test]
fn a_replaced_global_comes_back_on_the_next_call() {
    let (mut e, _host) = fresh();
    ok(&mut e, "print = 5; JSON.stringify = () => 'no'");
    assert_eq!(ok(&mut e, "print({a: 1}); 0"), "{\"a\":1}\n=> 0");
}

// ── rendering ────────────────────────────────────────────────────────────────

#[test]
fn every_rendering_rule() {
    let (mut e, _host) = fresh();
    let cases = [
        ("'text'", "\"text\""),
        ("'two\\nlines'", "\"two\\nlines\""),
        ("42", "42"),
        ("1.5", "1.5"),
        ("NaN", "null"),
        ("true", "true"),
        ("null", "null"),
        ("12345678901234567890n", "12345678901234567890n"),
        ("Symbol('s')", "Symbol(s)"),
        ("Symbol()", "Symbol()"),
        ("(function named() {})", "[function named]"),
        ("(() => {}).bind()", "[function bound ]"),
        (
            "Object.defineProperty(function () {}, 'name', {value: ''})",
            "[function]",
        ),
        ("new TypeError('bad')", "TypeError: bad"),
        (
            "({a: [1, 'b', null], c: {d: true}})",
            "{\"a\":[1,\"b\",null],\"c\":{\"d\":true}}",
        ),
        ("[1, [2, 3]]", "[1,[2,3]]"),
        (
            "({toJSON() { return undefined }, toString() { return 'S' }})",
            "S",
        ),
        ("[1n, 2n]", "1,2"),
        (
            "(() => { const o = {}; o.o = o; o.toString = () => 'cycle'; return o })()",
            "cycle",
        ),
        ("Object.create(null)", "{}"),
    ];
    for (code, want) in cases {
        assert_eq!(ok(&mut e, code), format!("=> {want}"), "{code}");
    }
    assert_eq!(
        ok(&mut e, "print('a', 1, undefined, null, [1], {b: 2}, 3n)"),
        "a 1 undefined null [1] {\"b\":2} 3n\n"
    );
    assert_eq!(ok(&mut e, "print()"), "\n");
    // print writes a string as it is; the `=> ` line quotes it.
    assert_eq!(ok(&mut e, "print('a\\nb'); 'a\\nb'"), "a\nb\n=> \"a\\nb\"");
}

#[test]
fn undefined_prints_no_value_line_and_nothing_is_empty() {
    let (mut e, _host) = fresh();
    assert_eq!(ok(&mut e, "undefined"), "");
    assert_eq!(ok(&mut e, "let x = 1"), "");
    assert_eq!(ok(&mut e, "print('p')"), "p\n");
    assert_eq!(ok(&mut e, "print('p'); 7"), "p\n=> 7");
}

// ── exceptions ───────────────────────────────────────────────────────────────

#[test]
fn an_error_returns_its_head_stack_and_what_was_printed() {
    let (mut e, _host) = fresh();
    let text = failed(
        &mut e,
        "print('before'); function f() { throw new Error('boom') } f()",
    );
    assert!(text.starts_with("Uncaught Error: boom\n"), "{text}");
    assert!(text.lines().any(|l| l.contains("at f")), "{text}");
    assert!(text.ends_with("before\n"), "{text}");
}

#[test]
fn a_thrown_non_error_is_rendered() {
    let (mut e, _host) = fresh();
    assert_eq!(
        failed(&mut e, "print('p'); throw {code: 3}"),
        "Uncaught {\"code\":3}\np\n"
    );
    assert_eq!(failed(&mut e, "throw 'plain'"), "Uncaught plain\n");
}

#[test]
fn top_level_await_is_a_syntax_error() {
    let (mut e, _host) = fresh();
    let text = failed(&mut e, "await Promise.resolve(1)");
    assert!(text.starts_with("Uncaught SyntaxError: "), "{text}");
}

#[test]
fn redeclaring_a_top_level_let_throws() {
    let (mut e, _host) = fresh();
    ok(&mut e, "let once = 1");
    let text = failed(&mut e, "let once = 2");
    assert!(text.starts_with("Uncaught SyntaxError: "), "{text}");
    assert_eq!(ok(&mut e, "once"), "=> 1");
}

#[test]
fn promise_jobs_run_in_the_same_call() {
    let (mut e, _host) = fresh();
    assert_eq!(
        ok(&mut e, "Promise.resolve(3).then(v => print(v))"),
        "3\n=> {}"
    );
    assert_eq!(
        ok(&mut e, "Promise.resolve(4).then(v => print(v)); undefined"),
        "4\n"
    );
    // Each call starts with empty printed text.
    assert_eq!(ok(&mut e, "1"), "=> 1");
}

// ── the cut ──────────────────────────────────────────────────────────────────

#[test]
fn the_cut_ends_on_a_utf8_boundary_and_is_marked() {
    let (mut e, _host) = fresh();
    // "é" is 2 bytes: 3 of them and a newline are 7 bytes; a limit of 5 falls inside the third,
    // so 4 bytes are kept.
    let out = e.run("print('ééé')", Some(5));
    assert_eq!(out.status, ResultStatus::Passed);
    assert!(out.truncated);
    assert_eq!(out.text, "éé\n[output cut: 7 bytes, the first 4 shown]");
    let out = e.run("print('x'.repeat(2000))", Some(512));
    assert_eq!(
        out.text,
        format!(
            "{}\n[output cut: 2001 bytes, the first 512 shown]",
            "x".repeat(512)
        )
    );
    let out = e.run("'abc'", Some(8));
    assert_eq!((out.text.as_str(), out.truncated), ("=> \"abc\"", false));
}

#[test]
fn no_limit_cuts_nothing() {
    let (mut e, _host) = fresh();
    let out = e.run("print('x'.repeat(200000))", None);
    assert_eq!(out.text.len(), 200_001);
    assert!(!out.truncated);
}

#[test]
fn a_failed_call_is_cut_too() {
    let (mut e, _host) = fresh();
    let out = e.run("throw new Error('y'.repeat(100))", Some(20));
    assert_eq!(out.status, ResultStatus::Failed);
    assert!(out.truncated);
    assert!(out.text.starts_with("Uncaught Error: yyyy"));
}

// ── config and input ─────────────────────────────────────────────────────────

#[test]
fn config_is_read_from_the_artifact_config() {
    let (mut e, _host) = fresh();
    let out = e.handle(
        Some(r#"{"code": "print('x'.repeat(20))"}"#),
        Some(r#"{"output_limit_bytes":10}"#),
    );
    assert_eq!(
        out.text,
        format!(
            "{}\n[output cut: 21 bytes, the first 10 shown]",
            "x".repeat(10)
        )
    );
    assert!(out.truncated);
    assert_eq!(e.parse_config(None), Ok(None));
    assert_eq!(e.parse_config(Some("{}")), Ok(None));
    assert_eq!(
        e.parse_config(Some(r#"{"output_limit_bytes":4294967295}"#)),
        Ok(Some(u32::MAX))
    );
}

#[test]
fn each_config_error_runs_nothing() {
    let (mut e, _host) = fresh();
    let cases = [
        ("not json", "the artifact config is not JSON"),
        ("[1]", "the artifact config must be a JSON object"),
        ("3", "the artifact config must be a JSON object"),
        (
            r#"{"limit":1}"#,
            "config key 'limit' is not known; the only key is output_limit_bytes",
        ),
        (
            r#"{"output_limit_bytes":0}"#,
            "output_limit_bytes must be an integer from 1 to 4294967295",
        ),
        (
            r#"{"output_limit_bytes":4294967296}"#,
            "output_limit_bytes must be an integer from 1 to 4294967295",
        ),
        (
            r#"{"output_limit_bytes":1.5}"#,
            "output_limit_bytes must be an integer from 1 to 4294967295",
        ),
        (
            r#"{"output_limit_bytes":"10"}"#,
            "output_limit_bytes must be an integer from 1 to 4294967295",
        ),
    ];
    for (config, want) in cases {
        let out = e.handle(Some(r#"{"code": "globalThis.ran = true"}"#), Some(config));
        assert_eq!(out.status, ResultStatus::Error, "{config}");
        assert!(
            out.text.starts_with("tool-repl-js: "),
            "{config}: {}",
            out.text
        );
        assert!(out.text.contains(want), "{config}: {}", out.text);
    }
    assert_eq!(ok(&mut e, "typeof ran"), "=> \"undefined\"");
}

#[test]
fn an_input_without_a_string_code_runs_nothing() {
    let (mut e, _host) = fresh();
    let cases: [(Option<&str>, &str); 6] = [
        (None, "is not JSON"),
        (Some("[]"), "must be a JSON object with a string `code`"),
        (
            Some("\"code\""),
            "must be a JSON object with a string `code`",
        ),
        (Some("{}"), "has no `code`"),
        (Some(r#"{"code": 1}"#), "`code` is not a string"),
        (Some("{"), "is not JSON"),
    ];
    for (data, want) in cases {
        let out = e.handle(data, None);
        assert_eq!(out.status, ResultStatus::Error, "{data:?}");
        assert!(out.text.contains(want), "{data:?}: {}", out.text);
    }
    let out = e.handle(Some(r#"{"code": "6 * 7"}"#), None);
    assert_eq!(
        (out.status, out.text.as_str()),
        (ResultStatus::Passed, "=> 42")
    );
}

// ── tools() ──────────────────────────────────────────────────────────────────

#[test]
fn tools_lists_operations_with_parsed_schemas() {
    let (mut e, h) = fresh();
    h.borrow_mut().list = Some(Ok(vec![
        Operation {
            name: "tool-read".into(),
            description: "Reads a file.".into(),
            input_schema: r#"{"type":"object"}"#.into(),
        },
        Operation {
            name: "odd".into(),
            description: "".into(),
            input_schema: "not json".into(),
        },
    ]));
    assert_eq!(
        ok(&mut e, "tools()"),
        r#"=> [{"name":"tool-read","description":"Reads a file.","inputSchema":{"type":"object"}},{"name":"odd","description":"","inputSchema":"not json"}]"#
    );
}

#[test]
fn a_refused_list_throws_kind_refused() {
    let (mut e, h) = fresh();
    h.borrow_mut().list = Some(Err("the bus answers only in the foreground".into()));
    assert_eq!(
        ok(
            &mut e,
            "try { tools() } catch (e) { [e instanceof Error, e.kind, e.message] }"
        ),
        r#"=> [true,"refused","the bus answers only in the foreground"]"#
    );
}

// ── call() ───────────────────────────────────────────────────────────────────

#[test]
fn call_returns_the_envelope() {
    let (mut e, h) = fresh();
    h.borrow_mut().answers.push((
        "tool-read".into(),
        Ok(Envelope {
            status: ResultStatus::Failed,
            summary: Some("sum".into()),
            data: None,
            data_path: Some("/x".into()),
            truncated: true,
            metadata: vec![
                ("k".into(), "1".into()),
                ("k".into(), "2".into()),
                ("j".into(), "3".into()),
            ],
            attachments: vec![Attachment {
                media_type: "image/png".into(),
                path: "a.png".into(),
            }],
        }),
    ));
    assert_eq!(
        ok(&mut e, "call('tool-read', {path: 'note.txt'})"),
        r#"=> {"status":"failed","summary":"sum","data":null,"dataPath":"/x","truncated":true,"metadata":{"k":"1","j":"3"},"attachments":[{"mediaType":"image/png","path":"a.png"}]}"#
    );
    assert_eq!(
        h.borrow().calls,
        vec![(
            "tool-read".to_string(),
            r#"{"path":"note.txt"}"#.to_string()
        )]
    );
}

#[test]
fn call_input_defaults_to_an_empty_object() {
    let (mut e, h) = fresh();
    h.borrow_mut().answers.push(("t".into(), Ok(passed("d"))));
    ok(
        &mut e,
        "call('t'); call('t', undefined); call('t', null); call('t', [1])",
    );
    let fake = h.borrow();
    let inputs: Vec<&str> = fake.calls.iter().map(|(_, i)| i.as_str()).collect();
    assert_eq!(inputs, ["{}", "{}", "null", "[1]"]);
}

#[test]
fn call_type_errors_reach_no_bus() {
    let (mut e, h) = fresh();
    for code in [
        "call()",
        "call(1)",
        "call('t', () => 1)",
        "call('t', Symbol('s'))",
        "call('t', 1n)",
        "(() => { const o = {}; o.o = o; return call('t', o) })()",
    ] {
        let text = failed(&mut e, code);
        assert!(
            text.starts_with("Uncaught TypeError: call(name, input): "),
            "{code}: {text}"
        );
    }
    assert!(h.borrow().calls.is_empty());
}

#[test]
fn call_errors_carry_their_kind() {
    let (mut e, h) = fresh();
    h.borrow_mut().answers.push((
        "gone".into(),
        Err(CallError::Failed("the tool trapped".into())),
    ));
    h.borrow_mut().answers.push((
        "bad".into(),
        Ok(Envelope {
            status: ResultStatus::Error,
            data: Some("tool said no".into()),
            ..passed("")
        }),
    ));
    h.borrow_mut().answers.push((
        "bad2".into(),
        Ok(Envelope {
            status: ResultStatus::Error,
            data: None,
            summary: Some("summary no".into()),
            ..passed("")
        }),
    ));
    h.borrow_mut().answers.push((
        "bad3".into(),
        Ok(Envelope {
            status: ResultStatus::Error,
            data: None,
            ..passed("")
        }),
    ));
    let probe = |name: &str| {
        format!("try {{ call('{name}', {{}}) }} catch (e) {{ [e instanceof Error, e.kind, e.message, e.result === undefined ? null : e.result.status] }}")
    };
    assert_eq!(
        ok(&mut e, &probe("tool-write")),
        r#"=> [true,"refused","'tool-write' is not an operation this program may call; list() returns every operation it may call",null]"#
    );
    assert_eq!(
        ok(&mut e, &probe("gone")),
        r#"=> [true,"failed","the tool trapped",null]"#
    );
    assert_eq!(
        ok(&mut e, &probe("bad")),
        r#"=> [true,"error","tool said no","error"]"#
    );
    assert_eq!(
        ok(&mut e, &probe("bad2")),
        r#"=> [true,"error","summary no","error"]"#
    );
    assert_eq!(
        ok(&mut e, &probe("bad3")),
        r#"=> [true,"error","tool returned status error","error"]"#
    );
    let text = failed(&mut e, "call('gone')");
    assert!(
        text.starts_with("Uncaught Error: the tool trapped\n"),
        "{text}"
    );
}

// ── callAll() ────────────────────────────────────────────────────────────────

#[test]
fn call_all_keeps_order_with_a_refused_entry_in_place() {
    let (mut e, h) = fresh();
    h.borrow_mut()
        .answers
        .push(("a".into(), Ok(passed("alpha"))));
    h.borrow_mut()
        .answers
        .push(("b".into(), Ok(passed("beta"))));
    assert_eq!(
        ok(&mut e, "callAll([['a', {path: 'a.txt'}], ['tool-write', {}], ['b']]).map(r => r instanceof Error ? r.kind : r.data)"
        ),
        r#"=> ["alpha","refused","beta"]"#
    );
    assert_eq!(
        h.borrow().batches,
        vec![vec![
            ("a".to_string(), r#"{"path":"a.txt"}"#.to_string()),
            ("tool-write".to_string(), "{}".to_string()),
            ("b".to_string(), "{}".to_string()),
        ]]
    );
    assert!(
        h.borrow().calls.is_empty(),
        "callAll makes one batch, not single calls"
    );
}

#[test]
fn call_all_entry_for_status_error_is_the_unthrown_error() {
    let (mut e, h) = fresh();
    h.borrow_mut().answers.push((
        "bad".into(),
        Ok(Envelope {
            status: ResultStatus::Error,
            data: Some("no".into()),
            ..passed("")
        }),
    ));
    assert_eq!(
        ok(&mut e, "const [r] = callAll([['bad', {}]]); [r instanceof Error, r.kind, r.message, r.result.data]"),
        r#"=> [true,"error","no","no"]"#
    );
}

#[test]
fn an_empty_call_all_makes_no_bus_call() {
    let (mut e, h) = fresh();
    assert_eq!(ok(&mut e, "callAll([])"), "=> []");
    assert!(h.borrow().batches.is_empty());
}

#[test]
fn call_all_type_errors_reach_no_bus() {
    let (mut e, h) = fresh();
    for code in [
        "callAll()",
        "callAll('a')",
        "callAll(['a'])",
        "callAll([[]])",
        "callAll([['a', {}, 3]])",
        "callAll([[1]])",
        "callAll([['a', {}], ['b', () => 1]])",
    ] {
        let text = failed(&mut e, code);
        assert!(
            text.starts_with("Uncaught TypeError: callAll(pairs): "),
            "{code}: {text}"
        );
    }
    assert!(h.borrow().batches.is_empty());
}

// ── run() ────────────────────────────────────────────────────────────────────

#[test]
fn run_maps_its_options_and_output() {
    let (mut e, h) = fresh();
    h.borrow_mut().run_answer = Some(Ok(RunOutput {
        exit_code: 3,
        timed_out: false,
        stdout: b"hi\n".to_vec(),
        stdout_truncated: true,
        stderr: vec![0xff, b'e'],
        stderr_truncated: false,
    }));
    assert_eq!(
        ok(&mut e, "run('echo', ['hi'], {env: {A: '1', B: 'two'}, stdin: 'é', timeoutMs: 250})"),
        "=> {\"exitCode\":3,\"timedOut\":false,\"stdout\":\"hi\\n\",\"stdoutTruncated\":true,\"stderr\":\"\u{FFFD}e\",\"stderrTruncated\":false}"
    );
    assert_eq!(
        h.borrow().runs,
        vec![RunRequest {
            program: "echo".into(),
            args: vec!["hi".into()],
            env: vec![("A".into(), "1".into()), ("B".into(), "two".into())],
            stdin: Some("é".as_bytes().to_vec()),
            timeout_ms: Some(250),
        }]
    );
    ok(&mut e, "run('true'); run('true', undefined, {})");
    assert_eq!(
        h.borrow().runs[1],
        RunRequest {
            program: "true".into(),
            ..RunRequest::default()
        }
    );
    assert_eq!(
        h.borrow().runs[2],
        RunRequest {
            program: "true".into(),
            ..RunRequest::default()
        }
    );
}

#[test]
fn run_type_errors_run_nothing() {
    let (mut e, h) = fresh();
    for code in [
        "run()",
        "run(1)",
        "run('a', 'b')",
        "run('a', [1])",
        "run('a', null)",
        "run('a', [], 3)",
        "run('a', [], [])",
        "run('a', [], {cwd: '/'})",
        "run('a', [], {env: []})",
        "run('a', [], {env: {A: 1}})",
        "run('a', [], {stdin: [1]})",
        "run('a', [], {timeoutMs: -1})",
        "run('a', [], {timeoutMs: 1.5})",
        "run('a', [], {timeoutMs: '5'})",
    ] {
        let text = failed(&mut e, code);
        assert!(
            text.starts_with("Uncaught TypeError: run(program, args, options): "),
            "{code}: {text}"
        );
    }
    assert!(h.borrow().runs.is_empty());
}

#[test]
fn run_errors_carry_kind_and_rule() {
    let (mut e, h) = fresh();
    let probe = "try { run('cat', ['x']) } catch (e) { [e instanceof Error, e.kind, e.rule === undefined ? null : e.rule, e.message] }";
    h.borrow_mut().run_answer = Some(Err(ProcError::Refused {
        rule: "capabilities.shell.allow".into(),
        reason: "binary 'cat' is not in capabilities.shell.allow".into(),
    }));
    assert_eq!(
        ok(&mut e, probe),
        r#"=> [true,"refused","capabilities.shell.allow","capabilities.shell.allow: binary 'cat' is not in capabilities.shell.allow"]"#
    );
    for (error, kind) in [
        (ProcError::UnknownHandle, "unknown-handle"),
        (ProcError::NotRunning, "not-running"),
        (ProcError::TimedOut, "timed-out"),
        (ProcError::Failed("spawn failed".into()), "failed"),
    ] {
        h.borrow_mut().run_answer = Some(Err(error));
        let text = ok(&mut e, probe);
        assert!(
            text.starts_with(&format!("=> [true,\"{kind}\",null,")),
            "{kind}: {text}"
        );
    }
    assert_eq!(
        ok(&mut e, "try { run('x') } catch (e) { e.message }"),
        "=> \"spawn failed\""
    );
}

#[test]
fn no_global_returns_a_promise() {
    let (mut e, h) = fresh();
    h.borrow_mut().answers.push(("t".into(), Ok(passed("d"))));
    assert_eq!(
        ok(&mut e, "[tools(), call('t'), callAll([['t']]), run('x'), print()].map(v => v instanceof Promise)"),
        "\n=> [false,false,false,false,false]"
    );
}
