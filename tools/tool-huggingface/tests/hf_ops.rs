//! Host tests for every op, against a scripted fake transport that records each request:
//! its method, URL, route (gateway or direct) and headers.
//!
//! After every call, [`Fake::assert_clean`] checks the invariants that hold for all of them:
//! only `GET` and `HEAD` were sent, and no request carried `authorization` or
//! `accept-encoding`.

use std::collections::VecDeque;
use std::fs;
use std::path::Path;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tool_huggingface::http::{BodySink, HttpFailure, Method, Reply, Transport};
use tool_huggingface::ops::{run, OpStatus, Response};

const GW: &str = "http://127.0.0.1:9";
const DS: &str = "https://datasets-server.example";
const CONFIG: &str =
    r#"{"config_version":1,"datasets_server_endpoint":"https://datasets-server.example"}"#;

#[derive(Debug, Clone)]
struct Seen {
    method: Method,
    url: String,
    gateway: bool,
    headers: Vec<(String, String)>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

type Answer = Result<Reply, HttpFailure>;

/// Answers requests in order from a script of `(method, url, answer)`, asserting each.
struct Fake {
    script: VecDeque<(Method, String, Answer)>,
    seen: Vec<Seen>,
}

impl Fake {
    fn new(script: Vec<(Method, &str, Answer)>) -> Self {
        Self {
            script: script
                .into_iter()
                .map(|(m, u, a)| (m, u.to_string(), a))
                .collect(),
            seen: Vec::new(),
        }
    }

    fn none() -> Self {
        Self::new(Vec::new())
    }

    fn next(&mut self, method: Method, url: &str, headers: &[(String, String)]) -> Answer {
        self.seen.push(Seen {
            method,
            url: url.to_string(),
            gateway: url.starts_with(GW),
            headers: headers.to_vec(),
        });
        let (want_method, want_url, answer) = self
            .script
            .pop_front()
            .unwrap_or_else(|| panic!("unscripted {method:?} {url}"));
        assert_eq!(method, want_method, "method of request to {url}");
        assert_eq!(url, want_url, "request order");
        answer
    }

    fn assert_clean(&self) {
        for seen in &self.seen {
            assert!(matches!(seen.method, Method::Get | Method::Head));
            for (name, _) in &seen.headers {
                assert_eq!(name, &name.to_ascii_lowercase());
                assert!(
                    name != "authorization" && name != "accept-encoding",
                    "{} carried {name}",
                    seen.url
                );
            }
        }
        assert!(self.script.is_empty(), "unused script: {:?}", self.script);
    }
}

impl Transport for Fake {
    fn request(
        &mut self,
        method: Method,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<Reply, HttpFailure> {
        let mut reply = self.next(method, url, headers)?;
        if method == Method::Head {
            reply.body.clear();
        }
        Ok(reply)
    }

    fn stream(
        &mut self,
        url: &str,
        headers: &[(String, String)],
        sink: &mut dyn BodySink,
    ) -> Result<Reply, HttpFailure> {
        let mut reply = self.next(Method::Get, url, headers)?;
        if (200..300).contains(&reply.status) {
            if sink.accept(reply.status, &reply.headers) {
                for chunk in reply.body.chunks(7) {
                    sink.write(chunk).map_err(HttpFailure::Aborted)?;
                }
            }
            reply.body.clear();
        }
        Ok(reply)
    }
}

fn reply(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Answer {
    Ok(Reply {
        status,
        headers: headers
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect(),
        body: body.to_vec(),
    })
}

fn ok_json(value: Value) -> Answer {
    reply(
        200,
        &[("content-type", "application/json")],
        value.to_string().as_bytes(),
    )
}

fn call(fake: &mut Fake, dir: &Path, config: Option<&str>, input: Value) -> Response {
    let response = run(Some(GW), config, &input.to_string(), fake, dir);
    fake.assert_clean();
    response
}

fn error_kind(response: &Response) -> String {
    let data: Value =
        serde_json::from_str(&response.data).unwrap_or_else(|e| panic!("{e}: {}", response.data));
    assert_eq!(data["ok"], json!(false));
    assert_eq!(response.summary, data["message"].as_str().unwrap());
    assert!(response.metadata.is_empty());
    data["error_kind"].as_str().unwrap().to_string()
}

fn data(response: &Response) -> Value {
    serde_json::from_str(&response.data).unwrap()
}

fn meta<'a>(response: &'a Response, key: &str) -> Option<&'a str> {
    response
        .metadata
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ── search ───────────────────────────────────────────────────────────────────

fn model_items(n: usize) -> Value {
    Value::Array(
        (0..n)
            .map(|i| {
                json!({
                    "id": format!("org/model-{i}"), "downloads": 100 - i, "likes": 7,
                    "lastModified": "2024-02-19T11:06:12.000Z", "pipeline_tag": "fill-mask",
                    "library_name": "transformers", "tags": ["bert", "license:apache-2.0"]
                })
            })
            .collect(),
    )
}

#[test]
fn search_models_maps_every_filter_and_renders_each_field() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "{GW}/api/models?search=bert%20base&limit=3&sort=lastModified&direction=-1&pipeline_tag=fill-mask&filter=onnx&filter=license%3Amit&filter=en&expand%5B%5D=downloads&expand%5B%5D=likes&expand%5B%5D=lastModified&expand%5B%5D=pipeline_tag&expand%5B%5D=library_name&expand%5B%5D=tags"
    );
    let mut fake = Fake::new(vec![(Method::Get, &url, ok_json(model_items(2)))]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"search","kind":"model","query":"  bert base ","task":"fill-mask","library":"onnx","license":"mit","tag":"en","sort":"updated","limit":3}),
    );
    assert_eq!(response.status, OpStatus::Passed, "{}", response.data);
    assert_eq!(response.summary, "2 models for \"bert base\"");
    assert!(response.data.starts_with(
        "Hugging Face search: models for \"bert base\" — 2 shown (limit 3); task fill-mask; library onnx; license mit; tag en; sort updated (descending)"
    ), "{}", response.data);
    assert!(response.data.contains(
        "\n\n[1] org/model-0\nfill-mask · transformers\ndownloads 100 · likes 7 · updated 2024-02-19T11:06:12.000Z · license apache-2.0"
    ), "{}", response.data);
    assert!(!response.truncated);
    assert_eq!(meta(&response, "state_effect"), Some("read"));
    assert_eq!(
        meta(&response, "resource_id"),
        Some("hf:search:model:bert base")
    );
    let seen = &fake.seen[0];
    assert!(seen.gateway);
    assert_eq!(seen.header("accept"), Some("application/json"));
    assert_eq!(seen.header("user-agent"), Some("tool-huggingface/0.1.0"));
}

#[test]
fn search_datasets_spaces_and_papers_use_their_own_fields() {
    let dir = tempfile::tempdir().unwrap();
    let ds_url = format!("{GW}/api/datasets?search=imdb&limit=10&filter=task_categories%3Atext-classification&filter=library%3Apolars&expand%5B%5D=downloads&expand%5B%5D=likes&expand%5B%5D=lastModified&expand%5B%5D=description&expand%5B%5D=tags");
    let mut fake = Fake::new(vec![(
        Method::Get,
        &ds_url,
        ok_json(
            json!([{"id":"stanfordnlp/imdb","description":"Large Movie Review Dataset.","downloads":5,"likes":1,"tags":["license:other"]}]),
        ),
    )]);
    let response = call(
        &mut fake,
        dir.path(),
        None,
        json!({"op":"search","kind":"dataset","query":"imdb","task":"text-classification","library":"polars"}),
    );
    assert_eq!(response.summary, "1 dataset for \"imdb\"");
    assert!(response
        .data
        .contains("\n[defaults in use — no config: block on this entry]"));
    assert!(response.data.contains("[1] stanfordnlp/imdb\nLarge Movie Review Dataset.\ndownloads 5 · likes 1 · license other"), "{}", response.data);

    let sp_url = format!("{GW}/api/spaces?search=whisper&limit=10&expand%5B%5D=likes&expand%5B%5D=lastModified&expand%5B%5D=cardData&expand%5B%5D=tags");
    let mut fake = Fake::new(vec![(
        Method::Get,
        &sp_url,
        ok_json(
            json!([{"id":"openai/whisper","likes":3,"cardData":{"title":"Whisper","short_description":"Speech to text"}}]),
        ),
    )]);
    let response = call(
        &mut fake,
        dir.path(),
        None,
        json!({"op":"search","kind":"space","query":"whisper"}),
    );
    assert!(
        response
            .data
            .contains("[1] openai/whisper\nSpeech to text\nlikes 3"),
        "{}",
        response.data
    );

    let pp_url = format!("{GW}/api/papers/search?q=attention&limit=2");
    let mut fake = Fake::new(vec![(
        Method::Get,
        &pp_url,
        ok_json(
            json!([{"paper":{"id":"1706.03762","title":"Attention Is All You Need","summary":"The dominant models.","upvotes":151,"publishedAt":"2017-06-12T17:57:34.000Z"}}]),
        ),
    )]);
    let response = call(
        &mut fake,
        dir.path(),
        None,
        json!({"op":"search","kind":"paper","query":"attention","limit":2}),
    );
    assert!(response.data.contains("[1] 1706.03762 — Attention Is All You Need\nThe dominant models.\nupvotes 151 · published 2017-06-12T17:57:34.000Z"), "{}", response.data);
}

#[test]
fn search_limit_is_clamped_and_said_so_and_extra_items_are_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("{GW}/api/models?search=x&limit=50&expand%5B%5D=downloads&expand%5B%5D=likes&expand%5B%5D=lastModified&expand%5B%5D=pipeline_tag&expand%5B%5D=library_name&expand%5B%5D=tags");
    let mut fake = Fake::new(vec![(Method::Get, &url, ok_json(model_items(60)))]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(r#"{"config_version":1,"max_output_bytes":65536}"#),
        json!({"op":"search","kind":"model","query":"x","limit":500}),
    );
    assert!(
        response
            .data
            .contains("50 shown (limit 50); limit clamped from 500 to 50"),
        "{}",
        response.data
    );
    assert!(!response.data.contains("org/model-50"));
}

#[test]
fn a_search_cut_at_the_budget_spills_the_full_response() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("{GW}/api/models?search=x&limit=50&expand%5B%5D=downloads&expand%5B%5D=likes&expand%5B%5D=lastModified&expand%5B%5D=pipeline_tag&expand%5B%5D=library_name&expand%5B%5D=tags");
    let items = model_items(50);
    let mut fake = Fake::new(vec![(Method::Get, &url, ok_json(items.clone()))]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(r#"{"config_version":1,"max_output_bytes":2048}"#),
        json!({"op":"search","kind":"model","query":"x","limit":50}),
    );
    assert_eq!(response.status, OpStatus::Passed);
    assert!(response.truncated);
    assert!(response.data.len() <= 2048, "{}", response.data.len());
    assert_eq!(
        response.data_path.as_deref(),
        Some("hf-results/search-1.json")
    );
    assert!(
        response
            .data
            .ends_with("full response in hf-results/search-1.json]"),
        "{}",
        response.data
    );
    let spilled: Value =
        serde_json::from_slice(&fs::read(dir.path().join("hf-results/search-1.json")).unwrap())
            .unwrap();
    assert_eq!(spilled, items);
    // A second cut search takes the next free name.
    let mut fake = Fake::new(vec![(Method::Get, &url, ok_json(items))]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(r#"{"config_version":1,"max_output_bytes":2048}"#),
        json!({"op":"search","kind":"model","query":"x","limit":50}),
    );
    assert_eq!(
        response.data_path.as_deref(),
        Some("hf-results/search-2.json")
    );
}

#[test]
fn a_failed_spill_still_marks_truncated_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("hf-results"), b"a file, not a folder").unwrap();
    let url = format!("{GW}/api/models?search=x&limit=50&expand%5B%5D=downloads&expand%5B%5D=likes&expand%5B%5D=lastModified&expand%5B%5D=pipeline_tag&expand%5B%5D=library_name&expand%5B%5D=tags");
    let mut fake = Fake::new(vec![(Method::Get, &url, ok_json(model_items(50)))]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(r#"{"config_version":1,"max_output_bytes":2048}"#),
        json!({"op":"search","kind":"model","query":"x","limit":50}),
    );
    assert!(response.truncated);
    assert_eq!(response.data_path, None);
    assert!(
        response
            .data
            .ends_with("the full response could not be saved]"),
        "{}",
        response.data
    );
}

#[test]
fn text_never_exceeds_the_budget_whatever_the_upstream_sends() {
    let dir = tempfile::tempdir().unwrap();
    for budget in [2048usize, 3000, 16384] {
        let config = format!(r#"{{"config_version":1,"max_output_bytes":{budget}}}"#);
        let huge = "ü".repeat(40_000);
        let url = format!("{GW}/api/datasets?search=x&limit=10&expand%5B%5D=downloads&expand%5B%5D=likes&expand%5B%5D=lastModified&expand%5B%5D=description&expand%5B%5D=tags");
        let items: Vec<Value> = (0..10)
            .map(|i| json!({"id": format!("{huge}{i}"), "description": huge}))
            .collect();
        let mut fake = Fake::new(vec![(Method::Get, &url, ok_json(Value::Array(items)))]);
        let response = call(
            &mut fake,
            dir.path(),
            Some(&config),
            json!({"op":"search","kind":"dataset","query":"x"}),
        );
        assert!(
            response.data.len() <= budget,
            "{} > {budget}",
            response.data.len()
        );
        assert!(response.truncated);

        let mut fake = Fake::new(vec![
            (
                Method::Get,
                &format!("{GW}/api/models/a/b?blobs=true"),
                ok_json(
                    json!({"id":"a/b","tags": (0..200).map(|i| format!("tag-{i}-{}", &huge[..200])).collect::<Vec<_>>(), "siblings": (0..500).map(|i| json!({"rfilename": format!("f{i}"), "size": i})).collect::<Vec<_>>()}),
                ),
            ),
            (
                Method::Get,
                &format!("{GW}/a/b/resolve/main/README.md"),
                reply(200, &[], huge.as_bytes()),
            ),
        ]);
        let response = call(
            &mut fake,
            dir.path(),
            Some(&config),
            json!({"op":"repo","repo":"a/b"}),
        );
        assert!(
            response.data.len() <= budget,
            "repo {} > {budget}",
            response.data.len()
        );
    }
}

// ── repo ─────────────────────────────────────────────────────────────────────

fn gpt2_meta() -> Value {
    json!({
        "id": "openai-community/gpt2", "private": false, "gated": false,
        "pipeline_tag": "text-generation", "library_name": "transformers",
        "tags": ["transformers", "license:mit"], "downloads": 34394889, "likes": 3958,
        "sha": "607a30d783dfa663caf39e06633721c8d4cfcd7e", "lastModified": "2024-02-19T10:57:45.000Z",
        "cardData": {"license": "mit"},
        "siblings": [
            {"rfilename": ".gitattributes", "size": 445},
            {"rfilename": "config.json", "size": 665},
            {"rfilename": "model.safetensors", "size": 548105171, "lfs": {"sha256": "248d"}}
        ]
    })
}

#[test]
fn repo_shows_metadata_files_and_the_card_following_the_relative_redirect() {
    let dir = tempfile::tempdir().unwrap();
    let mut fake = Fake::new(vec![
        (
            Method::Get,
            &format!("{GW}/api/models/openai-community/gpt2?blobs=true"),
            ok_json(gpt2_meta()),
        ),
        (
            Method::Get,
            &format!("{GW}/openai-community/gpt2/resolve/main/README.md"),
            reply(
                307,
                &[(
                    "location",
                    "/api/resolve-cache/models/openai-community/gpt2/607a/README.md?x=1",
                )],
                b"",
            ),
        ),
        (
            Method::Get,
            &format!("{GW}/api/resolve-cache/models/openai-community/gpt2/607a/README.md?x=1"),
            reply(
                200,
                &[],
                b"# GPT-2\nTest the whole generation capabilities here",
            ),
        ),
    ]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"repo","repo":"openai-community/gpt2"}),
    );
    assert_eq!(response.status, OpStatus::Passed, "{}", response.data);
    assert_eq!(response.summary, "model openai-community/gpt2");
    let expected = "Hugging Face repo: model openai-community/gpt2 @ main\nlicense: mit\ntags (2): transformers, license:mit\ngated: false\nprivate: false\nlast update: 2024-02-19T10:57:45.000Z\ncommit: 607a30d783dfa663caf39e06633721c8d4cfcd7e\ndownloads: 34394889\nlikes: 3958\npipeline: text-generation\nlibrary: transformers\nfiles (3, total 548106281 bytes):\n445  .gitattributes\n665  config.json\n548105171  model.safetensors\ncard:\n# GPT-2\nTest the whole generation capabilities here";
    assert_eq!(response.data, expected);
    assert!(fake.seen.iter().all(|s| s.gateway));
    assert_eq!(
        meta(&response, "resource_id"),
        Some("hf:repo:openai-community/gpt2")
    );
}

#[test]
fn repo_with_a_revision_a_dataset_and_an_unreadable_card() {
    let dir = tempfile::tempdir().unwrap();
    let mut fake = Fake::new(vec![
        (
            Method::Get,
            &format!("{GW}/api/datasets/org/data/revision/refs%2Fpr%2F1?blobs=true"),
            ok_json(json!({"id":"org/data","gated":"manual","private":false,"siblings":[]})),
        ),
        (
            Method::Get,
            &format!("{GW}/datasets/org/data/resolve/refs%2Fpr%2F1/README.md"),
            reply(401, &[("x-error-code", "GatedRepo")], b""),
        ),
    ]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"repo","kind":"dataset","repo":"org/data","revision":"refs/pr/1"}),
    );
    assert_eq!(response.status, OpStatus::Passed, "{}", response.data);
    assert!(response.data.contains("gated: manual"), "{}", response.data);
    assert!(
        response
            .data
            .ends_with("card: not readable (HTTP 401 GatedRepo)"),
        "{}",
        response.data
    );
}

#[test]
fn repo_files_and_card_are_cut_and_spilled() {
    let dir = tempfile::tempdir().unwrap();
    let mut meta_value = gpt2_meta();
    meta_value["siblings"] = Value::Array(
        (0..400)
            .map(|i| json!({"rfilename": format!("shard-{i:05}.bin"), "size": 1000}))
            .collect(),
    );
    let card = "c".repeat(20_000);
    let mut fake = Fake::new(vec![
        (
            Method::Get,
            &format!("{GW}/api/models/openai-community/gpt2?blobs=true"),
            ok_json(meta_value),
        ),
        (
            Method::Get,
            &format!("{GW}/openai-community/gpt2/resolve/main/README.md"),
            reply(200, &[], card.as_bytes()),
        ),
    ]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(r#"{"config_version":1,"max_output_bytes":4096}"#),
        json!({"op":"repo","repo":"openai-community/gpt2"}),
    );
    assert!(response.truncated);
    assert!(response.data.len() <= 4096);
    assert!(response.data.contains("more files"), "{}", response.data);
    let path = response.data_path.clone().unwrap();
    assert_eq!(path, "hf-results/repo-1.json");
    let spilled: Value = serde_json::from_slice(&fs::read(dir.path().join(path)).unwrap()).unwrap();
    assert_eq!(
        spilled["metadata"]["siblings"].as_array().unwrap().len(),
        400
    );
    assert_eq!(spilled["readme"].as_str().unwrap().len(), 20_000);
}

#[test]
fn not_found_is_a_404_a_not_found_code_or_a_keyless_401() {
    let dir = tempfile::tempdir().unwrap();
    for (status, headers) in [
        (
            401u16,
            vec![("x-error-message", "Invalid username or password.")],
        ),
        (404, vec![]),
        (401, vec![("x-error-code", "RepoNotFound")]),
    ] {
        let mut fake = Fake::new(vec![(
            Method::Get,
            &format!("{GW}/api/models/nope-xyz/none-abc?blobs=true"),
            reply(status, &headers, b"{\"error\":\"Repository not found\"}"),
        )]);
        let response = call(
            &mut fake,
            dir.path(),
            None,
            json!({"op":"repo","repo":"nope-xyz/none-abc"}),
        );
        assert_eq!(response.status, OpStatus::Failed);
        assert_eq!(error_kind(&response), "not_found", "{status} {headers:?}");
        assert_eq!(data(&response)["http_status"], json!(status));
    }
    // The message quotes x-error-message ahead of the body.
    let mut fake = Fake::new(vec![(
        Method::Get,
        &format!("{GW}/api/papers/9999.99999"),
        reply(404, &[], b"{\"error\":\"Paper not found.\"}"),
    )]);
    let response = call(
        &mut fake,
        dir.path(),
        None,
        json!({"op":"paper","arxiv_id":"9999.99999"}),
    );
    assert_eq!(error_kind(&response), "not_found");
    assert!(
        response.summary.ends_with("(HTTP 404): Paper not found"),
        "{}",
        response.summary
    );
}

#[test]
fn hub_status_codes_map_to_their_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("{GW}/api/models/a/b?blobs=true");
    type Case<'a> = (u16, Vec<(&'a str, &'a str)>, &'a str, OpStatus);
    let cases: Vec<Case> = vec![
        (
            403,
            vec![
                ("x-error-code", "GatedRepo"),
                ("x-error-message", "Access restricted"),
            ],
            "gated",
            OpStatus::Failed,
        ),
        (
            401,
            vec![("x-error-code", "GatedRepo")],
            "gated",
            OpStatus::Failed,
        ),
        (403, vec![], "upstream_unauthorized", OpStatus::Error),
        (
            401,
            vec![("x-error-code", "SomethingElse")],
            "upstream_unauthorized",
            OpStatus::Error,
        ),
        (
            429,
            vec![("retry-after", "30")],
            "rate_limited",
            OpStatus::Error,
        ),
        (500, vec![], "upstream_error", OpStatus::Error),
        (400, vec![], "upstream_error", OpStatus::Error),
    ];
    for (status, headers, want, want_status) in cases {
        let mut fake = Fake::new(vec![(
            Method::Get,
            &url,
            reply(status, &headers, b"{\"error\":\"nope\"}"),
        )]);
        let response = call(
            &mut fake,
            dir.path(),
            None,
            json!({"op":"repo","repo":"a/b"}),
        );
        assert_eq!(error_kind(&response), want, "{status} {headers:?}");
        assert_eq!(response.status, want_status);
        assert_eq!(data(&response)["http_status"], json!(status));
        if want == "rate_limited" {
            assert_eq!(data(&response)["retry_after"], json!("30"));
        }
        if want == "upstream_unauthorized" {
            assert!(
                response.summary.contains("gateway.api_key"),
                "{}",
                response.summary
            );
            assert!(
                response
                    .summary
                    .contains("credentials.HUGGINGFACE_ACCESS_TOKEN"),
                "{}",
                response.summary
            );
        }
    }
    // A keyless 401 on a search is not "not found".
    let search = format!("{GW}/api/papers/search?q=x&limit=10");
    let mut fake = Fake::new(vec![(Method::Get, &search, reply(401, &[], b""))]);
    let response = call(
        &mut fake,
        dir.path(),
        None,
        json!({"op":"search","kind":"paper","query":"x"}),
    );
    assert_eq!(error_kind(&response), "upstream_unauthorized");
}

#[test]
fn unparseable_or_oversized_bodies_are_response_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("{GW}/api/models/a/b?blobs=true");
    let mut fake = Fake::new(vec![(Method::Get, &url, reply(200, &[], b"<html>"))]);
    assert_eq!(
        error_kind(&call(
            &mut fake,
            dir.path(),
            None,
            json!({"op":"repo","repo":"a/b"})
        )),
        "response_invalid"
    );
    let mut fake = Fake::new(vec![(Method::Get, &url, Err(HttpFailure::TooLarge))]);
    assert_eq!(
        error_kind(&call(
            &mut fake,
            dir.path(),
            None,
            json!({"op":"repo","repo":"a/b"})
        )),
        "response_invalid"
    );
}

#[test]
fn transport_failures_name_the_denied_host() {
    let dir = tempfile::tempdir().unwrap();
    let mut fake = Fake::new(vec![(
        Method::Get,
        &format!("{DS}/splits?dataset=org%2Fdata"),
        Err(HttpFailure::Denied {
            host: "datasets-server.example".into(),
        }),
    )]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"dataset","repo":"org/data"}),
    );
    assert_eq!(error_kind(&response), "transport_error");
    assert_eq!(data(&response)["host"], json!("datasets-server.example"));
    assert!(
        response.summary.contains("capabilities.network.allow"),
        "{}",
        response.summary
    );
    let mut fake = Fake::new(vec![(
        Method::Get,
        &format!("{GW}/api/papers/1706.03762"),
        Err(HttpFailure::Transport("timeout".into())),
    )]);
    let response = call(
        &mut fake,
        dir.path(),
        None,
        json!({"op":"paper","arxiv_id":"1706.03762v5"}),
    );
    assert_eq!(error_kind(&response), "transport_error");
}

// ── paper ────────────────────────────────────────────────────────────────────

#[test]
fn paper_shows_title_authors_abstract_and_citing_repos() {
    let dir = tempfile::tempdir().unwrap();
    let mut fake = Fake::new(vec![
        (
            Method::Get,
            &format!("{GW}/api/papers/1706.03762"),
            ok_json(json!({
                "id":"1706.03762","title":"Attention Is All You Need",
                "authors":[{"name":"Ashish Vaswani"},{"name":"Noam Shazeer"}],
                "publishedAt":"2017-06-12T17:57:34.000Z","upvotes":151,
                "summary":"The dominant sequence transduction models.","numTotalModels":330,"numTotalDatasets":1
            })),
        ),
        (
            Method::Get,
            &format!(
                "{GW}/api/models?filter=arxiv%3A1706.03762&sort=downloads&direction=-1&limit=10"
            ),
            ok_json(json!([{"id":"google/x","downloads":5},{"id":"nvidia/y","downloads":4}])),
        ),
        (
            Method::Get,
            &format!(
                "{GW}/api/datasets?filter=arxiv%3A1706.03762&sort=downloads&direction=-1&limit=10"
            ),
            ok_json(json!([{"id":"d/z","downloads":1}])),
        ),
    ]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"paper","arxiv_id":"1706.03762v7"}),
    );
    assert_eq!(response.status, OpStatus::Passed, "{}", response.data);
    assert_eq!(
        response.summary,
        "paper 1706.03762: Attention Is All You Need"
    );
    assert_eq!(response.data, "Hugging Face paper: 1706.03762\ntitle: Attention Is All You Need\nauthors (2): Ashish Vaswani, Noam Shazeer\npublished: 2017-06-12T17:57:34.000Z\nupvotes: 151\nabstract:\nThe dominant sequence transduction models.\nmodels citing it (2 shown of 330, by downloads):\ngoogle/x (downloads 5)\nnvidia/y (downloads 4)\ndatasets citing it (1 shown of 1, by downloads):\nd/z (downloads 1)");
    assert_eq!(meta(&response, "resource_id"), Some("hf:paper:1706.03762"));
}

// ── dataset ──────────────────────────────────────────────────────────────────

fn splits() -> Value {
    json!({"splits":[
        {"dataset":"org/data","config":"default","split":"train"},
        {"dataset":"org/data","config":"default","split":"test"},
        {"dataset":"org/data","config":"other","split":"train"}
    ],"pending":[],"failed":[]})
}

fn rows(n: usize, text: &str) -> Value {
    json!({
        "features":[
            {"feature_idx":0,"name":"text","type":{"dtype":"string","_type":"Value"}},
            {"feature_idx":1,"name":"label","type":{"names":["neg","pos"],"_type":"ClassLabel"}}
        ],
        "rows": (0..n).map(|i| json!({"row_idx": i, "row": {"text": text, "label": 1}, "truncated_cells": []})).collect::<Vec<_>>(),
        "num_rows_total": 8530, "num_rows_per_page": 100, "partial": false
    })
}

#[test]
fn dataset_page_shows_splits_schema_and_rows_all_direct() {
    let dir = tempfile::tempdir().unwrap();
    let mut fake = Fake::new(vec![
        (
            Method::Get,
            &format!("{DS}/splits?dataset=org%2Fdata"),
            ok_json(splits()),
        ),
        (
            Method::Get,
            &format!("{DS}/rows?dataset=org%2Fdata&config=default&split=train&offset=0&length=2"),
            ok_json(rows(2, "good film")),
        ),
    ]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"dataset","repo":"org/data","limit":2}),
    );
    assert_eq!(response.status, OpStatus::Passed, "{}", response.data);
    assert_eq!(response.data, "Hugging Face dataset: org/data — default/train; rows 0–1 (of 8530)\nsplits (3): default/train, default/test, other/train\nschema:\ntext: string\nlabel: ClassLabel [neg, pos]\nrows (2 returned):\n[row 0] {\"label\":1,\"text\":\"good film\"}\n[row 1] {\"label\":1,\"text\":\"good film\"}");
    assert_eq!(
        response.summary,
        "2 rows of org/data default/train (of 8530)"
    );
    assert!(
        fake.seen.iter().all(|s| !s.gateway),
        "the datasets server is direct"
    );
}

#[test]
fn dataset_search_and_filter_pick_the_named_split() {
    let dir = tempfile::tempdir().unwrap();
    let mut fake = Fake::new(vec![
        (Method::Get, &format!("{DS}/splits?dataset=org%2Fdata"), ok_json(splits())),
        (Method::Get, &format!("{DS}/search?dataset=org%2Fdata&config=default&split=test&query=brilliant&offset=5&length=10"), ok_json(rows(1, "brilliant"))),
    ]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"dataset","repo":"org/data","split":"test","query":"brilliant","offset":5}),
    );
    assert!(response.data.starts_with("Hugging Face dataset: org/data — default/test; search \"brilliant\"; rows 5–5 of the matches (of 8530)"), "{}", response.data);

    let mut fake = Fake::new(vec![
        (Method::Get, &format!("{DS}/splits?dataset=org%2Fdata"), ok_json(splits())),
        (Method::Get, &format!("{DS}/filter?dataset=org%2Fdata&config=other&split=train&where=%22label%22%3D1&orderby=%22text%22%20DESC&offset=0&length=10"), ok_json(rows(1, "x"))),
    ]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"dataset","repo":"org/data","config":"other","where":"\"label\"=1","order_by":"\"text\" DESC"}),
    );
    assert!(response.data.starts_with("Hugging Face dataset: org/data — other/train; where \"label\"=1; order by \"text\" DESC; rows 0–0 of the matches"), "{}", response.data);

    let mut fake = Fake::new(vec![(
        Method::Get,
        &format!("{DS}/splits?dataset=org%2Fdata"),
        ok_json(splits()),
    )]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"dataset","repo":"org/data","split":"nope"}),
    );
    assert_eq!(error_kind(&response), "not_found");
}

#[test]
fn dataset_rows_are_cut_and_spilled() {
    let dir = tempfile::tempdir().unwrap();
    let long = "w".repeat(3000);
    let mut fake = Fake::new(vec![
        (
            Method::Get,
            &format!("{DS}/splits?dataset=org%2Fdata"),
            ok_json(splits()),
        ),
        (
            Method::Get,
            &format!("{DS}/rows?dataset=org%2Fdata&config=default&split=train&offset=0&length=100"),
            ok_json(rows(100, &long)),
        ),
    ]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(
            r#"{"config_version":1,"datasets_server_endpoint":"https://datasets-server.example","max_output_bytes":8192}"#,
        ),
        json!({"op":"dataset","repo":"org/data","limit":1000}),
    );
    assert!(response.truncated);
    assert!(response.data.len() <= 8192);
    assert!(response.data.contains("[limit clamped from 1000 to 100]"));
    assert!(response.data.contains("[row 0] "));
    assert!(
        response.data.contains("row strings cut to 1000 bytes"),
        "{}",
        response.data
    );
    let path = response.data_path.clone().unwrap();
    assert_eq!(path, "hf-results/dataset-1.json");
    let spilled: Value = serde_json::from_slice(&fs::read(dir.path().join(path)).unwrap()).unwrap();
    assert_eq!(spilled["rows"]["rows"].as_array().unwrap().len(), 100);
    assert_eq!(spilled["splits"], splits());
}

#[test]
fn a_viewer_that_cannot_serve_is_viewer_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let mut fake = Fake::new(vec![(Method::Get, &format!("{DS}/splits?dataset=org%2Fdata"), reply(501, &[("x-error-code", "DatasetWithScriptNotSupportedError")], b"{\"error\":\"The dataset viewer doesn't support this dataset because it runs arbitrary python code.\"}"))]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"dataset","repo":"org/data"}),
    );
    assert_eq!(error_kind(&response), "viewer_unavailable");
    assert_eq!(response.status, OpStatus::Failed);
    assert!(
        response.summary.contains("arbitrary python code"),
        "{}",
        response.summary
    );

    let mut fake = Fake::new(vec![(
        Method::Get,
        &format!("{DS}/splits?dataset=org%2Fdata"),
        ok_json(
            json!({"splits":[],"pending":[],"failed":[{"error":{"error":"Cannot load the dataset split."}}]}),
        ),
    )]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"dataset","repo":"org/data"}),
    );
    assert_eq!(error_kind(&response), "viewer_unavailable");
    assert!(
        response.summary.contains("Cannot load the dataset split."),
        "{}",
        response.summary
    );

    let mut fake = Fake::new(vec![
        (Method::Get, &format!("{DS}/splits?dataset=org%2Fdata"), ok_json(splits())),
        (Method::Get, &format!("{DS}/filter?dataset=org%2Fdata&config=default&split=train&where=x&offset=0&length=10"), reply(500, &[("x-error-code", "ResponseNotReady")], b"{\"error\":\"the dataset index is loading, this can take a minute\"}")),
    ]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"dataset","repo":"org/data","where":"x"}),
    );
    assert_eq!(error_kind(&response), "viewer_unavailable");
    assert!(
        response.summary.contains("index is loading"),
        "{}",
        response.summary
    );

    let mut fake = Fake::new(vec![
        (Method::Get, &format!("{DS}/splits?dataset=org%2Fdata"), ok_json(splits())),
        (Method::Get, &format!("{DS}/filter?dataset=org%2Fdata&config=default&split=train&where=bad&offset=0&length=10"), reply(422, &[("x-error-code", "InvalidParameter")], b"{\"error\":\"Parameter 'where' contains errors\"}")),
    ]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"dataset","repo":"org/data","where":"bad"}),
    );
    assert_eq!(error_kind(&response), "upstream_error");
    assert!(response
        .summary
        .contains("Parameter 'where' contains errors"));
}

#[test]
fn a_viewer_401_is_classified_through_the_hub() {
    let dir = tempfile::tempdir().unwrap();
    let viewer_401 = || {
        reply(401, &[("x-error-code", "ExternalUnauthenticatedError")], b"{\"error\":\"The dataset does not exist, or is not accessible without authentication (private or gated).\"}")
    };
    let cases: Vec<(Answer, &str)> = vec![
        (
            ok_json(json!({"id":"org/data","gated":"auto","private":false})),
            "gated",
        ),
        (
            ok_json(json!({"id":"org/data","gated":false,"private":true})),
            "upstream_unauthorized",
        ),
        (
            reply(
                401,
                &[("x-error-message", "Invalid username or password.")],
                b"",
            ),
            "not_found",
        ),
        (reply(404, &[], b""), "not_found"),
    ];
    for (hub, want) in cases {
        let mut fake = Fake::new(vec![
            (
                Method::Get,
                &format!("{DS}/splits?dataset=org%2Fdata"),
                viewer_401(),
            ),
            (Method::Get, &format!("{GW}/api/datasets/org/data"), hub),
        ]);
        let response = call(
            &mut fake,
            dir.path(),
            Some(CONFIG),
            json!({"op":"dataset","repo":"org/data"}),
        );
        assert_eq!(error_kind(&response), want);
        assert!(
            response.summary.contains("without a credential"),
            "{}",
            response.summary
        );
        assert!(
            response.summary.contains("op download"),
            "{}",
            response.summary
        );
        assert!(!fake.seen[0].gateway && fake.seen[1].gateway);
    }
}

// ── download ─────────────────────────────────────────────────────────────────

const CONFIG_JSON: &[u8] = b"{\n  \"activation_function\": \"gelu_new\"\n}\n";

fn resolve(file: &str) -> String {
    format!("{GW}/openai-community/gpt2/resolve/main/{file}")
}

#[test]
fn download_heads_then_gets_through_the_relative_redirect_and_hashes() {
    let dir = tempfile::tempdir().unwrap();
    let size = CONFIG_JSON.len().to_string();
    let cache =
        format!("{GW}/api/resolve-cache/models/openai-community/gpt2/607a/config.json?etag=1");
    let mut fake = Fake::new(vec![
        (
            Method::Head,
            &resolve("config.json"),
            reply(
                307,
                &[
                    (
                        "location",
                        "/api/resolve-cache/models/openai-community/gpt2/607a/config.json?etag=1",
                    ),
                    ("x-linked-size", &size),
                    (
                        "x-linked-etag",
                        "\"10c66461e4c109db5a2196bff4bb59be30396ed8\"",
                    ),
                ],
                b"",
            ),
        ),
        (
            Method::Get,
            &resolve("config.json"),
            reply(
                307,
                &[(
                    "location",
                    "/api/resolve-cache/models/openai-community/gpt2/607a/config.json?etag=1",
                )],
                b"",
            ),
        ),
        (
            Method::Get,
            &cache,
            reply(200, &[("content-length", &size)], CONFIG_JSON),
        ),
    ]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"download","repo":"openai-community/gpt2","repo_file":"config.json","dest":"dl/config.json"}),
    );
    assert_eq!(response.status, OpStatus::Passed, "{}", response.data);
    let written = fs::read(dir.path().join("dl/config.json")).unwrap();
    assert_eq!(written, CONFIG_JSON);
    let digest = sha256_hex(CONFIG_JSON);
    assert_eq!(response.data, format!(
        "Hugging Face download: model openai-community/gpt2 config.json @ main\ndest: dl/config.json\nbytes: {size} (whole file)\nsha256: {digest}\nfetched: Hub via gateway (credential attached by the runtime when one is bound) → Hub via gateway (credential attached by the runtime when one is bound)"
    ));
    assert_eq!(
        response.summary,
        format!("downloaded dl/config.json ({size} bytes)")
    );
    assert_eq!(meta(&response, "state_effect"), Some("mutate"));
    assert_eq!(meta(&response, "resource_id"), Some("dl/config.json"));
    assert_eq!(fake.seen[0].method, Method::Head);
    assert!(fake.seen.iter().all(|s| s.gateway));
    assert!(!dir.path().join("dl/config.json.partial").exists());
}

#[test]
fn an_lfs_download_follows_the_cdn_redirect_directly_and_checks_the_etag() {
    let dir = tempfile::tempdir().unwrap();
    let blob = vec![42u8; 4096];
    let digest = sha256_hex(&blob);
    let etag = format!("\"{digest}\"");
    let cdn = "https://cdn.example/xet-bridge-us/abc?X-Amz-Signature=secret";
    let mut fake = Fake::new(vec![
        (
            Method::Head,
            &resolve("model.safetensors"),
            reply(
                302,
                &[
                    ("location", cdn),
                    ("x-linked-size", "4096"),
                    ("x-linked-etag", &etag),
                ],
                b"",
            ),
        ),
        (
            Method::Get,
            &resolve("model.safetensors"),
            reply(302, &[("location", cdn)], b""),
        ),
        (Method::Get, cdn, reply(200, &[], &blob)),
    ]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"download","repo":"openai-community/gpt2","repo_file":"model.safetensors","dest":"model.safetensors"}),
    );
    assert_eq!(response.status, OpStatus::Passed, "{}", response.data);
    assert!(
        response.data.contains(&format!(
            "sha256: {digest} (matches the Hub's x-linked-etag)"
        )),
        "{}",
        response.data
    );
    assert!(
        response
            .data
            .ends_with("→ cdn.example direct (no credential)"),
        "{}",
        response.data
    );
    assert!(
        !response.data.contains("Signature"),
        "the signed query never appears"
    );
    assert!(!fake.seen[2].gateway);
    assert_eq!(
        sha256_hex(&fs::read(dir.path().join("model.safetensors")).unwrap()),
        digest
    );

    // The same file whose bytes do not hash to the etag is removed.
    let dir = tempfile::tempdir().unwrap();
    let mut fake = Fake::new(vec![
        (
            Method::Head,
            &resolve("model.safetensors"),
            reply(
                302,
                &[
                    ("location", cdn),
                    ("x-linked-size", "4096"),
                    ("x-linked-etag", &etag),
                ],
                b"",
            ),
        ),
        (
            Method::Get,
            &resolve("model.safetensors"),
            reply(302, &[("location", cdn)], b""),
        ),
        (Method::Get, cdn, reply(200, &[], &[7u8; 4096])),
    ]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"download","repo":"openai-community/gpt2","repo_file":"model.safetensors","dest":"model.safetensors"}),
    );
    assert_eq!(error_kind(&response), "response_invalid");
    assert!(!dir.path().join("model.safetensors").exists());
    assert!(!dir.path().join("model.safetensors.partial").exists());
}

#[test]
fn a_ranged_download_carries_range_on_every_hop_and_needs_a_206() {
    let dir = tempfile::tempdir().unwrap();
    let cdn = "https://cdn.example/blob?sig=1";
    let part = vec![1u8; 1024];
    let mut fake = Fake::new(vec![
        (
            Method::Head,
            &resolve("model.safetensors"),
            reply(
                302,
                &[("location", cdn), ("x-linked-size", "548105171")],
                b"",
            ),
        ),
        (
            Method::Get,
            &resolve("model.safetensors"),
            reply(302, &[("location", cdn)], b""),
        ),
        (
            Method::Get,
            cdn,
            reply(
                206,
                &[("content-range", "bytes 1000-2023/548105171")],
                &part,
            ),
        ),
    ]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"download","repo":"openai-community/gpt2","repo_file":"model.safetensors","dest":"part.bin","byte_offset":1000,"byte_length":1024}),
    );
    assert_eq!(response.status, OpStatus::Passed, "{}", response.data);
    assert!(
        response
            .data
            .contains("bytes: 1024 (range 1000–2023 of 548105171)"),
        "{}",
        response.data
    );
    assert_eq!(
        fake.seen[0].header("range"),
        None,
        "the HEAD carries no range"
    );
    for seen in &fake.seen[1..] {
        assert_eq!(
            seen.header("range"),
            Some("bytes=1000-2023"),
            "{}",
            seen.url
        );
    }
    assert_eq!(
        fs::metadata(dir.path().join("part.bin")).unwrap().len(),
        1024
    );

    // A 200 to a range is refused and nothing is left behind.
    let dir = tempfile::tempdir().unwrap();
    let mut fake = Fake::new(vec![
        (
            Method::Head,
            &resolve("model.safetensors"),
            reply(
                302,
                &[("location", cdn), ("x-linked-size", "548105171")],
                b"",
            ),
        ),
        (
            Method::Get,
            &resolve("model.safetensors"),
            reply(302, &[("location", cdn)], b""),
        ),
        (Method::Get, cdn, reply(200, &[], &part)),
    ]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"download","repo":"openai-community/gpt2","repo_file":"model.safetensors","dest":"part.bin","byte_offset":0,"byte_length":1024}),
    );
    assert_eq!(error_kind(&response), "response_invalid");
    assert!(!dir.path().join("part.bin").exists() && !dir.path().join("part.bin.partial").exists());

    // A range past the end is clamped to the file; an offset at the end is invalid_input.
    let mut fake = Fake::new(vec![(
        Method::Head,
        &resolve("config.json"),
        reply(200, &[("content-length", "665")], b""),
    )]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"download","repo":"openai-community/gpt2","repo_file":"config.json","dest":"x","byte_offset":665,"byte_length":10}),
    );
    assert_eq!(error_kind(&response), "invalid_input");
    let mut fake = Fake::new(vec![
        (
            Method::Head,
            &resolve("config.json"),
            reply(200, &[("content-length", "665")], b""),
        ),
        (
            Method::Get,
            &resolve("config.json"),
            reply(206, &[("content-range", "bytes 660-664/665")], b"12345"),
        ),
    ]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"download","repo":"openai-community/gpt2","repo_file":"config.json","dest":"tail","byte_offset":660,"byte_length":100}),
    );
    assert_eq!(response.status, OpStatus::Passed, "{}", response.data);
    assert!(response.data.contains("bytes: 5 (range 660–664 of 665)"));
}

#[test]
fn over_and_under_runs_remove_the_partial_file() {
    for body in [vec![0u8; 100], vec![0u8; 50]] {
        let dir = tempfile::tempdir().unwrap();
        let mut fake = Fake::new(vec![
            (
                Method::Head,
                &resolve("f.bin"),
                reply(200, &[("content-length", "64")], b""),
            ),
            (Method::Get, &resolve("f.bin"), reply(200, &[], &body)),
        ]);
        let response = call(
            &mut fake,
            dir.path(),
            Some(CONFIG),
            json!({"op":"download","repo":"openai-community/gpt2","repo_file":"f.bin","dest":"out/f.bin"}),
        );
        assert_eq!(error_kind(&response), "response_invalid", "{}", body.len());
        assert!(!dir.path().join("out/f.bin").exists());
        assert!(!dir.path().join("out/f.bin.partial").exists());
    }
}

#[test]
fn too_large_sends_only_heads_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut fake = Fake::new(vec![(
        Method::Head,
        &resolve("model.safetensors"),
        reply(
            302,
            &[
                ("location", "https://cdn.example/x"),
                ("x-linked-size", "548105171"),
            ],
            b"",
        ),
    )]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"download","repo":"openai-community/gpt2","repo_file":"model.safetensors","dest":"m.bin"}),
    );
    assert_eq!(error_kind(&response), "too_large");
    assert_eq!(response.status, OpStatus::Failed);
    assert_eq!(data(&response)["size"], json!(548105171u64));
    assert_eq!(data(&response)["cap"], json!(104857600u64));
    assert!(response.summary.contains("548105171") && response.summary.contains("104857600"));
    assert!(fake.seen.iter().all(|s| s.method == Method::Head));
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);

    // HEAD hops are followed until a size is reported.
    let mut fake = Fake::new(vec![
        (
            Method::Head,
            &resolve("model.safetensors"),
            reply(302, &[("location", "https://cdn.example/x")], b""),
        ),
        (
            Method::Head,
            "https://cdn.example/x",
            reply(200, &[("content-length", "548105171")], b""),
        ),
    ]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(r#"{"config_version":1,"max_download_bytes":1000}"#),
        json!({"op":"download","repo":"openai-community/gpt2","repo_file":"model.safetensors","dest":"m.bin"}),
    );
    assert_eq!(error_kind(&response), "too_large");
    assert!(fake.seen.iter().all(|s| s.method == Method::Head));
}

#[test]
fn no_reported_size_is_response_invalid_with_nothing_fetched() {
    let dir = tempfile::tempdir().unwrap();
    let mut fake = Fake::new(vec![(Method::Head, &resolve("f"), reply(200, &[], b""))]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"download","repo":"openai-community/gpt2","repo_file":"f","dest":"f"}),
    );
    assert_eq!(error_kind(&response), "response_invalid");
    assert_eq!(fake.seen.len(), 1);
}

#[test]
fn download_errors_classify_like_reads() {
    let dir = tempfile::tempdir().unwrap();
    let gated = format!("{GW}/meta-llama/Llama-2-7b-hf/resolve/main/config.json");
    let mut fake = Fake::new(vec![(
        Method::Head,
        &gated,
        reply(
            401,
            &[
                ("x-error-code", "GatedRepo"),
                (
                    "x-error-message",
                    "Access to model meta-llama/Llama-2-7b-hf is restricted.",
                ),
            ],
            b"",
        ),
    )]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"download","repo":"meta-llama/Llama-2-7b-hf","repo_file":"config.json","dest":"c.json"}),
    );
    assert_eq!(error_kind(&response), "gated");
    assert!(
        response.summary.contains("is restricted"),
        "{}",
        response.summary
    );
    let mut fake = Fake::new(vec![(
        Method::Head,
        &resolve("nope.json"),
        reply(404, &[("x-error-code", "EntryNotFound")], b""),
    )]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"download","repo":"openai-community/gpt2","repo_file":"nope.json","dest":"n.json"}),
    );
    assert_eq!(error_kind(&response), "not_found");
    // A denied CDN hop names the host.
    let mut fake = Fake::new(vec![
        (
            Method::Head,
            &resolve("m"),
            reply(
                302,
                &[
                    ("location", "https://cdn.example/x"),
                    ("x-linked-size", "10"),
                ],
                b"",
            ),
        ),
        (
            Method::Get,
            &resolve("m"),
            reply(302, &[("location", "https://cdn.example/x")], b""),
        ),
        (
            Method::Get,
            "https://cdn.example/x",
            Err(HttpFailure::Denied {
                host: "cdn.example".into(),
            }),
        ),
    ]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"download","repo":"openai-community/gpt2","repo_file":"m","dest":"m"}),
    );
    assert_eq!(error_kind(&response), "transport_error");
    assert_eq!(data(&response)["host"], json!("cdn.example"));
    assert!(!dir.path().join("m").exists() && !dir.path().join("m.partial").exists());
}

#[test]
fn download_paths_for_datasets_spaces_revisions_and_nested_files_are_encoded() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("{GW}/datasets/org/data/resolve/v1.0/plain%20text/train-00000.parquet");
    let mut fake = Fake::new(vec![
        (
            Method::Head,
            &url,
            reply(200, &[("content-length", "3")], b""),
        ),
        (Method::Get, &url, reply(200, &[], b"abc")),
    ]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"download","kind":"dataset","repo":"org/data","revision":"v1.0","repo_file":"plain text/train-00000.parquet","dest":"a/b/c.parquet"}),
    );
    assert_eq!(response.status, OpStatus::Passed, "{}", response.data);
    assert_eq!(fs::read(dir.path().join("a/b/c.parquet")).unwrap(), b"abc");
    let url = format!("{GW}/spaces/org/app/resolve/main/app.py");
    let mut fake = Fake::new(vec![
        (
            Method::Head,
            &url,
            reply(200, &[("content-length", "1")], b""),
        ),
        (Method::Get, &url, reply(200, &[], b"x")),
    ]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"download","kind":"space","repo":"org/app","repo_file":"app.py","dest":"app.py"}),
    );
    assert_eq!(response.status, OpStatus::Passed, "{}", response.data);
}

// ── redirects ────────────────────────────────────────────────────────────────

#[test]
fn redirect_rule_cases() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("{GW}/api/papers/1706.03762");
    let refused: Vec<(&str, Vec<(&str, &str)>)> = vec![
        ("no location", vec![]),
        (
            "gateway authority",
            vec![("location", "http://127.0.0.1:9/api/papers/x")],
        ),
        (
            "gateway authority over https",
            vec![("location", "https://127.0.0.1:9/x")],
        ),
        (
            "plain http off loopback",
            vec![("location", "http://cdn.example/x")],
        ),
        ("other scheme", vec![("location", "ftp://cdn.example/x")]),
    ];
    for (what, headers) in refused {
        let mut fake = Fake::new(vec![(Method::Get, &url, reply(302, &headers, b""))]);
        let response = call(
            &mut fake,
            dir.path(),
            None,
            json!({"op":"paper","arxiv_id":"1706.03762"}),
        );
        assert_eq!(error_kind(&response), "response_invalid", "{what}");
        assert_eq!(fake.seen.len(), 1, "{what}: not followed");
    }

    // Three redirects are followed; a fourth is refused.
    let hops: Vec<String> = (1..=4).map(|i| format!("{GW}/hop{i}")).collect();
    let mut script = vec![(
        Method::Get,
        url.as_str(),
        reply(302, &[("location", "/hop1")], b""),
    )];
    script.push((
        Method::Get,
        hops[0].as_str(),
        reply(302, &[("location", "/hop2")], b""),
    ));
    script.push((
        Method::Get,
        hops[1].as_str(),
        reply(302, &[("location", "/hop3")], b""),
    ));
    script.push((
        Method::Get,
        hops[2].as_str(),
        reply(302, &[("location", "/hop4")], b""),
    ));
    let mut fake = Fake::new(script);
    let response = call(
        &mut fake,
        dir.path(),
        None,
        json!({"op":"paper","arxiv_id":"1706.03762"}),
    );
    assert_eq!(error_kind(&response), "response_invalid");
    assert!(
        response.summary.contains("more than 3 redirects"),
        "{}",
        response.summary
    );

    // An absolute https Location naming the Hub host goes direct, with no credential.
    let mut fake = Fake::new(vec![
        (
            Method::Get,
            &url,
            reply(
                301,
                &[("location", "https://hub.example/api/papers/1706.03762")],
                b"",
            ),
        ),
        (
            Method::Get,
            "https://hub.example/api/papers/1706.03762",
            reply(200, &[], b"{\"id\":\"1706.03762\",\"title\":\"T\"}"),
        ),
        (
            Method::Get,
            &format!(
                "{GW}/api/models?filter=arxiv%3A1706.03762&sort=downloads&direction=-1&limit=10"
            ),
            ok_json(json!([])),
        ),
        (
            Method::Get,
            &format!(
                "{GW}/api/datasets?filter=arxiv%3A1706.03762&sort=downloads&direction=-1&limit=10"
            ),
            ok_json(json!([])),
        ),
    ]);
    let response = call(
        &mut fake,
        dir.path(),
        None,
        json!({"op":"paper","arxiv_id":"1706.03762"}),
    );
    assert_eq!(response.status, OpStatus::Passed, "{}", response.data);
    assert!(!fake.seen[1].gateway);

    // A relative Location from a direct request stays on that host; plain-http loopback is followed.
    let mut fake = Fake::new(vec![
        (
            Method::Get,
            &format!("{DS}/splits?dataset=org%2Fdata"),
            reply(307, &[("location", "splits2?dataset=org%2Fdata")], b""),
        ),
        (
            Method::Get,
            &format!("{DS}/splits2?dataset=org%2Fdata"),
            reply(302, &[("location", "http://localhost:8/s")], b""),
        ),
        (Method::Get, "http://localhost:8/s", ok_json(splits())),
        (
            Method::Get,
            &format!("{DS}/rows?dataset=org%2Fdata&config=default&split=train&offset=0&length=10"),
            ok_json(rows(1, "x")),
        ),
    ]);
    let response = call(
        &mut fake,
        dir.path(),
        Some(CONFIG),
        json!({"op":"dataset","repo":"org/data"}),
    );
    assert_eq!(response.status, OpStatus::Passed, "{}", response.data);
    assert!(fake.seen.iter().all(|s| !s.gateway));
}

// ── refusals send nothing ────────────────────────────────────────────────────

#[test]
fn every_input_refusal_sends_nothing() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("exists.json"), b"x").unwrap();
    fs::write(dir.path().join("afile"), b"x").unwrap();
    fs::write(dir.path().join("p.bin.partial"), b"x").unwrap();
    let dl = |dest: &str| json!({"op":"download","repo":"a/b","repo_file":"f","dest":dest});
    let cases: Vec<Value> = vec![
        json!({}),
        json!([1]),
        json!({"op":"write"}),
        json!({"op":"search","query":"x"}),
        json!({"op":"search","kind":"model"}),
        json!({"op":"search","kind":"model","query":"   "}),
        json!({"op":"search","kind":"model","query":"x".repeat(201)}),
        json!({"op":"search","kind":"space","query":"x","task":"t"}),
        json!({"op":"search","kind":"paper","query":"x","sort":"likes"}),
        json!({"op":"search","kind":"model","query":"x","license":"a\u{7}b"}),
        json!({"op":"search","kind":"model","query":"x","tag":"t".repeat(101)}),
        json!({"op":"search","kind":"model","query":"x","sort":"newest"}),
        json!({"op":"search","kind":"model","query":"x","limit":0}),
        json!({"op":"search","kind":"model","query":"x","repo":"a/b"}),
        json!({"op":"search","kind":"model","query":"x","unknown":1}),
        json!({"op":"repo"}),
        json!({"op":"repo","repo":"a/b","kind":"paper"}),
        json!({"op":"repo","repo":"../etc"}),
        json!({"op":"repo","repo":"a/b/c"}),
        json!({"op":"repo","repo":"a--b/c"}),
        json!({"op":"repo","repo":"-a/b"}),
        json!({"op":"repo","repo":"a/b","revision":"../x"}),
        json!({"op":"repo","repo":"a/b","revision":"a b"}),
        json!({"op":"repo","repo":"a/b","dest":"x"}),
        json!({"op":"dataset","repo":"a/b","query":"x","where":"y"}),
        json!({"op":"dataset","repo":"a/b","order_by":"y"}),
        json!({"op":"dataset","repo":"a/b","offset":-1}),
        json!({"op":"dataset","repo":"a/b","where":"w".repeat(1001)}),
        json!({"op":"paper"}),
        json!({"op":"paper","arxiv_id":"1706.037"}),
        json!({"op":"paper","arxiv_id":"cs/0101001"}),
        json!({"op":"paper","arxiv_id":"1706.03762vx"}),
        json!({"op":"download","repo":"a/b","dest":"x"}),
        json!({"op":"download","repo":"a/b","repo_file":"f"}),
        json!({"op":"download","repo":"a/b","repo_file":"/etc/passwd","dest":"x"}),
        json!({"op":"download","repo":"a/b","repo_file":"a/../b","dest":"x"}),
        json!({"op":"download","repo":"a/b","repo_file":"a\\b","dest":"x"}),
        json!({"op":"download","repo":"a/b","repo_file":"f","dest":"x","byte_offset":0}),
        json!({"op":"download","repo":"a/b","repo_file":"f","dest":"x","byte_offset":-1,"byte_length":1}),
        json!({"op":"download","repo":"a/b","repo_file":"f","dest":"x","byte_offset":0,"byte_length":0}),
        json!({"op":"download","repo":"a/b","repo_file":"f","dest":"x","byte_offset":0,"byte_length":104857601}),
        dl("/etc/passwd"),
        dl("C:x"),
        dl("../x"),
        dl("a/../x"),
        dl("./x"),
        dl("a//x"),
        dl("dir/"),
        dl("a\\b"),
        dl("a\u{0}b"),
        dl("a\nb"),
        dl(&"n".repeat(256)),
        dl(&format!("{}/x", "d/".repeat(512))),
        dl(""),
        dl("exists.json"),
        dl("p.bin"),
        dl("afile/x"),
    ];
    for input in cases {
        let mut fake = Fake::none();
        let response = call(&mut fake, dir.path(), None, input.clone());
        assert_eq!(error_kind(&response), "invalid_input", "{input}");
        assert_eq!(response.status, OpStatus::Failed);
        assert!(fake.seen.is_empty(), "{input}: nothing sent");
    }
    // A null counts as absent; a twice-encoded object is read once more.
    let mut fake = Fake::new(vec![(
        Method::Get,
        &format!("{GW}/api/papers/1706.03762"),
        reply(404, &[], b""),
    )]);
    let response = call(
        &mut fake,
        dir.path(),
        None,
        Value::String(json!({"op":"paper","arxiv_id":"1706.03762","repo":null}).to_string()),
    );
    assert_eq!(error_kind(&response), "not_found");
}

#[test]
fn config_refusals_and_a_missing_gateway_send_nothing() {
    let dir = tempfile::tempdir().unwrap();
    for config in [
        "{}",
        r#"{"config_version":1,"max_output_bytes":100}"#,
        r#"{"config_version":1,"api_key":"hf_x"}"#,
        r#"{"config_version":1,"datasets_server_endpoint":"http://evil.example"}"#,
    ] {
        let mut fake = Fake::none();
        let response = call(
            &mut fake,
            dir.path(),
            Some(config),
            json!({"op":"paper","arxiv_id":"1706.03762"}),
        );
        assert_eq!(error_kind(&response), "config_invalid", "{config}");
        assert_eq!(response.status, OpStatus::Error);
    }
    for gateway in [None, Some(""), Some("   ")] {
        for input in [
            json!({"op":"paper","arxiv_id":"1706.03762"}),
            json!({"op":"dataset","repo":"a/b"}),
            json!({"op":"search","kind":"model","query":"x"}),
            json!({"op":"repo","repo":"a/b"}),
            json!({"op":"download","repo":"a/b","repo_file":"f","dest":"f"}),
        ] {
            let mut fake = Fake::none();
            let response = run(gateway, None, &input.to_string(), &mut fake, dir.path());
            assert_eq!(error_kind(&response), "gateway_missing");
            assert_eq!(response.status, OpStatus::Error);
            assert!(
                response
                    .summary
                    .contains("api_key: ${HUGGINGFACE_ACCESS_TOKEN}"),
                "{}",
                response.summary
            );
            assert!(response.summary.contains("keyless: true"));
            assert!(response
                .summary
                .contains("mur config set -g credentials.HUGGINGFACE_ACCESS_TOKEN <token>"));
            assert!(response
                .summary
                .contains("endpoint: https://huggingface.co"));
            assert!(fake.seen.is_empty());
        }
    }
}

#[test]
fn a_local_write_failure_is_write_failed_and_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    // A parent that appears as a file between the dest check and the write.
    struct Sabotage<'a>(Fake, &'a Path);
    impl Transport for Sabotage<'_> {
        fn request(
            &mut self,
            m: Method,
            u: &str,
            h: &[(String, String)],
        ) -> Result<Reply, HttpFailure> {
            let r = self.0.request(m, u, h);
            fs::write(self.1.join("late"), b"file").unwrap();
            r
        }
        fn stream(
            &mut self,
            u: &str,
            h: &[(String, String)],
            s: &mut dyn BodySink,
        ) -> Result<Reply, HttpFailure> {
            self.0.stream(u, h, s)
        }
    }
    let mut fake = Sabotage(
        Fake::new(vec![
            (
                Method::Head,
                &resolve("f"),
                reply(200, &[("content-length", "3")], b""),
            ),
            (Method::Get, &resolve("f"), reply(200, &[], b"abc")),
        ]),
        dir.path(),
    );
    let input =
        json!({"op":"download","repo":"openai-community/gpt2","repo_file":"f","dest":"late/f"});
    let response = run(Some(GW), None, &input.to_string(), &mut fake, dir.path());
    fake.0.assert_clean();
    assert_eq!(error_kind(&response), "write_failed");
}
