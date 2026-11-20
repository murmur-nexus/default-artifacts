//! `search`, `repo` and `paper`: Hub API reads, every request a gateway request.
//!
//! Search parameters, as verified against the live Hub on 2026-11-20:
//!
//! | Input | models | datasets |
//! |---|---|---|
//! | `task` | `pipeline_tag=<task>` | `filter=task_categories:<task>` |
//! | `library` | `filter=<library>` | `filter=library:<library>` |
//! | `license` | `filter=license:<license>` | `filter=license:<license>` |
//! | `tag` | `filter=<tag>` | `filter=<tag>` |
//! | `sort` | `sort=<field>&direction=-1` | `sort=<field>&direction=-1` |
//!
//! Repeated `filter` parameters are ANDed by the Hub. `expand[]` asks for exactly the fields
//! an item shows, because a plain list omits `lastModified` for models and `cardData` for
//! Spaces.

use serde_json::{json, Map, Value};

use crate::http::{encode, encode_path, query_string, Method, Route, USER_AGENT};
use crate::input::{Kind, PaperInput, RepoInput, SearchInput, DEFAULT_REVISION};
use crate::ops::{
    body_value, failure, hub_failure, kind, parse_json, read_success, spill, Ctx, Response,
};
use crate::render::{cut, cut_chars, finish, Page};

/// A search item's description, in characters.
pub const DESCRIPTION_CHARS: usize = 200;
/// Tags shown by `repo` before `+N more`.
pub const MAX_TAGS: usize = 50;
/// Authors shown by `paper` before `+N`.
pub const MAX_AUTHORS: usize = 50;
/// Linked models or datasets listed by `paper`.
pub const PAPER_LINKS: u32 = 10;

// ── search ───────────────────────────────────────────────────────────────────

/// The search URL's path and query, exactly as sent.
pub fn search_path(input: &SearchInput) -> String {
    let mut pairs: Vec<(&str, String)> = Vec::new();
    if input.kind == Kind::Paper {
        pairs.push(("q", input.query.clone()));
        pairs.push(("limit", input.limit.value.to_string()));
        return format!("/api/papers/search{}", query_string(&pairs));
    }
    pairs.push(("search", input.query.clone()));
    pairs.push(("limit", input.limit.value.to_string()));
    if let Some(sort) = input.sort {
        pairs.push(("sort", sort.hub_field().to_string()));
        pairs.push(("direction", "-1".to_string()));
    }
    let dataset = input.kind == Kind::Dataset;
    if let Some(task) = &input.task {
        if dataset {
            pairs.push(("filter", format!("task_categories:{task}")));
        } else {
            pairs.push(("pipeline_tag", task.clone()));
        }
    }
    if let Some(library) = &input.library {
        if dataset {
            pairs.push(("filter", format!("library:{library}")));
        } else {
            pairs.push(("filter", library.clone()));
        }
    }
    if let Some(license) = &input.license {
        pairs.push(("filter", format!("license:{license}")));
    }
    if let Some(tag) = &input.tag {
        pairs.push(("filter", tag.clone()));
    }
    let expand: &[&str] = match input.kind {
        Kind::Model => &[
            "downloads",
            "likes",
            "lastModified",
            "pipeline_tag",
            "library_name",
            "tags",
        ],
        Kind::Dataset => &["downloads", "likes", "lastModified", "description", "tags"],
        Kind::Space => &["likes", "lastModified", "cardData", "tags"],
        Kind::Paper => &[],
    };
    for field in expand {
        pairs.push(("expand[]", field.to_string()));
    }
    format!("/api/{}{}", input.kind.plural(), query_string(&pairs))
}

/// One search hit, as shown.
struct Hit {
    id: String,
    title: Option<String>,
    description: Option<String>,
    stats: Vec<String>,
}

pub fn search(ctx: &mut Ctx, input: &SearchInput) -> Response {
    let url = ctx.hub(&search_path(input));
    let fetched = match ctx.get_api(&url, Route::Gateway) {
        Ok(fetched) => fetched,
        Err(response) => return response,
    };
    let reply = fetched.reply;
    if !(200..300).contains(&reply.status) {
        return hub_failure(
            reply.status,
            &reply.headers,
            &reply.body,
            "the search",
            false,
        );
    }
    let value = match parse_json(&reply.body, "the Hub's search response") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Some(items) = value.as_array() else {
        return failure(
            kind::RESPONSE_INVALID,
            "the Hub's search response is not a list",
            Map::new(),
        );
    };
    // The server is asked for `limit`; anything past it is not shown.
    let hits: Vec<Hit> = items
        .iter()
        .take(input.limit.value as usize)
        .map(|item| hit(input.kind, item))
        .collect();

    let plural = input.kind.plural();
    let noun = if hits.len() == 1 {
        input.kind.as_str()
    } else {
        plural
    };
    let mut head = format!(
        "Hugging Face search: {plural} for \"{}\" — {} shown (limit {})",
        input.query,
        hits.len(),
        input.limit.value
    );
    if let Some(from) = input.limit.clamped_from {
        head.push_str(&format!(
            "; limit clamped from {from} to {}",
            input.limit.value
        ));
    }
    for (name, value) in [
        ("task", &input.task),
        ("library", &input.library),
        ("license", &input.license),
        ("tag", &input.tag),
    ] {
        if let Some(value) = value {
            head.push_str(&format!("; {name} {value}"));
        }
    }
    if let Some(sort) = input.sort {
        head.push_str(&format!("; sort {} (descending)", sort.as_str()));
    }
    head.push_str(ctx.defaults_line());

    let render = |limit: usize| {
        let mut page = Page::new(limit);
        page.push(&head, "header");
        if hits.is_empty() {
            page.push("\n\nno results", "body");
            return page;
        }
        let mut shown = 0;
        for (index, hit) in hits.iter().enumerate() {
            let (block, cut_description) = hit_block(index, hit);
            if cut_description {
                page.note(format!(
                    "descriptions cut to {DESCRIPTION_CHARS} characters"
                ));
            }
            if !page.try_push(&block) {
                if shown == 0 {
                    page.push(&block, "first result");
                    shown = 1;
                }
                break;
            }
            shown += 1;
        }
        if shown < hits.len() {
            page.note(format!("{shown} of {} results shown", hits.len()));
        }
        page
    };
    let out_dir = ctx.out_dir;
    let rendered = finish(ctx.config.max_output_bytes, render, || {
        spill(out_dir, "search", &reply.body)
    });
    read_success(
        "search",
        &format!("{}:{}", input.kind.as_str(), input.query),
        format!("{} {noun} for \"{}\"", hits.len(), input.query),
        rendered,
    )
}

fn hit(kind: Kind, item: &Value) -> Hit {
    let text = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_string);
    if kind == Kind::Paper {
        let paper = item.get("paper").unwrap_or(item);
        let title = text(paper.get("title")).or_else(|| text(item.get("title")));
        let summary = text(paper.get("summary")).or_else(|| text(item.get("summary")));
        let mut stats = Vec::new();
        if let Some(upvotes) = paper.get("upvotes").filter(|v| v.is_number()) {
            stats.push(format!("upvotes {upvotes}"));
        }
        if let Some(published) =
            text(paper.get("publishedAt")).or_else(|| text(item.get("publishedAt")))
        {
            stats.push(format!("published {published}"));
        }
        return Hit {
            id: text(paper.get("id")).unwrap_or_default(),
            title,
            description: summary,
            stats,
        };
    }
    let description = match kind {
        Kind::Model => {
            let parts: Vec<String> = ["pipeline_tag", "library_name"]
                .iter()
                .filter_map(|key| text(item.get(*key)))
                .collect();
            (!parts.is_empty()).then(|| parts.join(" · "))
        }
        Kind::Dataset => text(item.get("description")),
        Kind::Space => {
            let card = item.get("cardData");
            text(card.and_then(|c| c.get("short_description")))
                .or_else(|| text(card.and_then(|c| c.get("title"))))
        }
        Kind::Paper => None,
    };
    let mut stats = Vec::new();
    for (key, label) in [("downloads", "downloads"), ("likes", "likes")] {
        if let Some(value) = item.get(key).filter(|v| v.is_number()) {
            stats.push(format!("{label} {value}"));
        }
    }
    if let Some(updated) = text(item.get("lastModified")) {
        stats.push(format!("updated {updated}"));
    }
    if let Some(license) = license_of(item) {
        stats.push(format!("license {license}"));
    }
    Hit {
        id: text(item.get("id")).unwrap_or_default(),
        title: None,
        description,
        stats,
    }
}

fn hit_block(index: usize, hit: &Hit) -> (String, bool) {
    let mut block = format!("\n\n[{}] {}", index + 1, hit.id);
    if let Some(title) = &hit.title {
        block.push_str(&format!(" — {title}"));
    }
    let mut was_cut = false;
    if let Some(description) = &hit.description {
        let (description, cut) = cut_chars(description, DESCRIPTION_CHARS);
        was_cut = cut;
        block.push('\n');
        block.push_str(&description);
    }
    if !hit.stats.is_empty() {
        block.push('\n');
        block.push_str(&hit.stats.join(" · "));
    }
    (block, was_cut)
}

/// A repo's license: the first `license:<id>` tag, else `cardData.license`.
fn license_of(item: &Value) -> Option<String> {
    let from_tags = item.get("tags").and_then(Value::as_array).and_then(|tags| {
        tags.iter()
            .filter_map(Value::as_str)
            .find_map(|tag| tag.strip_prefix("license:"))
            .map(str::to_string)
    });
    from_tags.or_else(
        || match item.get("cardData").and_then(|c| c.get("license")) {
            Some(Value::String(license)) => Some(license.clone()),
            Some(Value::Array(list)) => {
                let names: Vec<&str> = list.iter().filter_map(Value::as_str).collect();
                (!names.is_empty()).then(|| names.join(", "))
            }
            _ => None,
        },
    )
}

// ── repo ─────────────────────────────────────────────────────────────────────

/// The metadata URL's path and query: `?blobs=true` makes `siblings` carry each file's size,
/// recursively, in the one response (the tree endpoint lists one directory per request).
pub fn repo_path(kind: Kind, repo: &str, revision: Option<&str>) -> String {
    let revision = revision
        .map(|rev| format!("/revision/{}", encode(rev)))
        .unwrap_or_default();
    format!("/api/{}/{repo}{revision}?blobs=true", kind.plural())
}

/// The resolve URL's path for one file.
pub fn resolve_path(kind: Kind, repo: &str, revision: &str, file: &str) -> String {
    format!(
        "/{}{repo}/resolve/{}/{}",
        kind.repo_prefix(),
        encode(revision),
        encode_path(file)
    )
}

pub fn repo(ctx: &mut Ctx, input: &RepoInput) -> Response {
    let subject = format!("{} {}", input.kind.as_str(), input.repo);
    let url = ctx.hub(&repo_path(
        input.kind,
        &input.repo,
        input.revision.as_deref(),
    ));
    let fetched = match ctx.get_api(&url, Route::Gateway) {
        Ok(fetched) => fetched,
        Err(response) => return response,
    };
    let reply = fetched.reply;
    if !(200..300).contains(&reply.status) {
        return hub_failure(reply.status, &reply.headers, &reply.body, &subject, true);
    }
    let meta = match parse_json(&reply.body, "the Hub's repo metadata") {
        Ok(Value::Object(meta)) => Value::Object(meta),
        Ok(_) => {
            return failure(
                kind::RESPONSE_INVALID,
                "the Hub's repo metadata is not an object",
                Map::new(),
            )
        }
        Err(response) => return response,
    };

    let revision = input.revision.as_deref().unwrap_or(DEFAULT_REVISION);
    let card_url = ctx.hub(&resolve_path(
        input.kind,
        &input.repo,
        revision,
        "README.md",
    ));
    let headers = vec![("user-agent".to_string(), USER_AGENT.to_string())];
    let card: Result<String, String> =
        match ctx
            .client
            .fetch(Method::Get, &card_url, Route::Gateway, &headers, &|_| false)
        {
            Ok(fetched) if (200..300).contains(&fetched.reply.status) => {
                Ok(String::from_utf8_lossy(&fetched.reply.body).into_owned())
            }
            Ok(fetched) => {
                let code = fetched.reply.header("x-error-code").unwrap_or_default();
                Err(if code.is_empty() {
                    format!("HTTP {}", fetched.reply.status)
                } else {
                    format!("HTTP {} {code}", fetched.reply.status)
                })
            }
            Err(error) => Err(match error {
                crate::http::ChainError::Redirect(message) => message,
                crate::http::ChainError::Http { failure, .. } => format!("{failure:?}"),
            }),
        };

    let text = |key: &str| meta.get(key).and_then(Value::as_str).map(str::to_string);
    let mut head = format!("Hugging Face repo: {subject} @ {revision}");
    head.push_str(ctx.defaults_line());
    if let Some(license) = license_of(&meta) {
        head.push_str(&format!("\nlicense: {license}"));
    }
    let tags: Vec<&str> = meta
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| tags.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let tags_cut = tags.len() > MAX_TAGS;
    if !tags.is_empty() {
        let shown = tags
            .iter()
            .take(MAX_TAGS)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        head.push_str(&format!("\ntags ({}): {shown}", tags.len()));
        if tags_cut {
            head.push_str(&format!(" +{} more", tags.len() - MAX_TAGS));
        }
    }
    for (key, label) in [("gated", "gated"), ("private", "private")] {
        if let Some(value) = meta.get(key) {
            let shown = value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_string);
            head.push_str(&format!("\n{label}: {shown}"));
        }
    }
    if let Some(updated) = text("lastModified") {
        head.push_str(&format!("\nlast update: {updated}"));
    }
    if let Some(sha) = text("sha") {
        head.push_str(&format!("\ncommit: {sha}"));
    }
    for (key, label) in [("downloads", "downloads"), ("likes", "likes")] {
        if let Some(value) = meta.get(key).filter(|v| v.is_number()) {
            head.push_str(&format!("\n{label}: {value}"));
        }
    }
    for (key, label) in [
        ("pipeline_tag", "pipeline"),
        ("library_name", "library"),
        ("sdk", "sdk"),
    ] {
        if let Some(value) = text(key) {
            head.push_str(&format!("\n{label}: {value}"));
        }
    }

    let files: Vec<(Option<u64>, String)> = meta
        .get("siblings")
        .and_then(Value::as_array)
        .map(|siblings| {
            siblings
                .iter()
                .map(|s| {
                    (
                        s.get("size").and_then(Value::as_u64),
                        s.get("rfilename")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let total: u64 = files.iter().filter_map(|(size, _)| *size).sum();
    let all_sized = files.iter().all(|(size, _)| size.is_some());
    let files_head = if all_sized {
        format!("\nfiles ({}, total {total} bytes):", files.len())
    } else {
        format!(
            "\nfiles ({}, total {total} bytes of the files with a reported size):",
            files.len()
        )
    };

    let render = |limit: usize| {
        let mut page = Page::new(limit);
        page.push(&head, "header");
        if tags_cut {
            page.note(format!("tags beyond {MAX_TAGS} omitted"));
        }
        page.push(&files_head, "file list");
        let file_room = page.room() / 2;
        let start = page.text.len();
        let mut shown = 0;
        for (size, path) in &files {
            let size = size.map_or_else(|| "?".to_string(), |s| s.to_string());
            let line = format!("\n{size}  {path}");
            if page.text.len() - start + line.len() > file_room || !page.try_push(&line) {
                break;
            }
            shown += 1;
        }
        if shown < files.len() {
            let more = format!("\n… {} more files", files.len() - shown);
            page.try_push(&more);
            page.note(format!("{shown} of {} files listed", files.len()));
        }
        match &card {
            Ok(card) => {
                page.push("\ncard:\n", "card");
                if card.len() > page.room() {
                    let room = page.room();
                    page.text.push_str(&cut(card, room));
                    page.note(format!("card cut to {room} bytes"));
                } else {
                    page.text.push_str(card);
                }
            }
            Err(reason) => page.push(&format!("\ncard: not readable ({reason})"), "card"),
        }
        page
    };
    let out_dir = ctx.out_dir;
    let rendered = finish(ctx.config.max_output_bytes, render, || {
        let full = json!({
            "metadata": meta,
            "readme": card.as_ref().ok(),
        });
        spill(out_dir, "repo", full.to_string().as_bytes())
    });
    read_success("repo", &input.repo, subject.clone(), rendered)
}

// ── paper ────────────────────────────────────────────────────────────────────

/// The paper URL and the two citing-repo list URLs, exactly as sent.
pub fn paper_paths(arxiv_id: &str) -> [String; 3] {
    let list = |plural: &str| {
        format!(
            "/api/{plural}{}",
            query_string(&[
                ("filter", format!("arxiv:{arxiv_id}")),
                ("sort", "downloads".to_string()),
                ("direction", "-1".to_string()),
                ("limit", PAPER_LINKS.to_string()),
            ])
        )
    };
    [
        format!("/api/papers/{arxiv_id}"),
        list("models"),
        list("datasets"),
    ]
}

/// A repo citing a paper: its id and download count.
type Linked = (String, Option<u64>);

pub fn paper(ctx: &mut Ctx, input: &PaperInput) -> Response {
    let id = &input.arxiv_id;
    let subject = format!("paper {id}");
    let [paper_path, models_path, datasets_path] = paper_paths(id);
    let url = ctx.hub(&paper_path);
    let fetched = match ctx.get_api(&url, Route::Gateway) {
        Ok(fetched) => fetched,
        Err(response) => return response,
    };
    let reply = fetched.reply;
    if !(200..300).contains(&reply.status) {
        return hub_failure(reply.status, &reply.headers, &reply.body, &subject, true);
    }
    let paper = match parse_json(&reply.body, "the Hub's paper response") {
        Ok(paper) => paper,
        Err(response) => return response,
    };

    // The citing lists are context; one that cannot be read is reported in place.
    let mut list = |path: &str| -> (Result<Vec<Linked>, String>, Value) {
        let url = ctx.hub(path);
        match ctx.get_api(&url, Route::Gateway) {
            Ok(fetched) if (200..300).contains(&fetched.reply.status) => {
                let value = body_value(&fetched.reply.body);
                match value.as_array() {
                    Some(items) => (
                        Ok(items
                            .iter()
                            .map(|item| {
                                (
                                    item.get("id")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_string(),
                                    item.get("downloads").and_then(Value::as_u64),
                                )
                            })
                            .collect()),
                        value,
                    ),
                    None => (Err("the response is not a list".to_string()), value),
                }
            }
            Ok(fetched) => (
                Err(format!("HTTP {}", fetched.reply.status)),
                body_value(&fetched.reply.body),
            ),
            Err(response) => (Err(response.summary), Value::Null),
        }
    };
    let (models, models_raw) = list(&models_path);
    let (datasets, datasets_raw) = list(&datasets_path);

    let text = |key: &str| paper.get(key).and_then(Value::as_str).map(str::to_string);
    let title = text("title").unwrap_or_default();
    let mut head = format!("Hugging Face paper: {id}");
    head.push_str(ctx.defaults_line());
    head.push_str(&format!("\ntitle: {title}"));
    let authors: Vec<&str> = paper
        .get("authors")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|a| a.get("name").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    let authors_cut = authors.len() > MAX_AUTHORS;
    if !authors.is_empty() {
        head.push_str(&format!(
            "\nauthors ({}): {}",
            authors.len(),
            authors
                .iter()
                .take(MAX_AUTHORS)
                .copied()
                .collect::<Vec<_>>()
                .join(", ")
        ));
        if authors_cut {
            head.push_str(&format!(" +{}", authors.len() - MAX_AUTHORS));
        }
    }
    if let Some(published) = text("publishedAt") {
        head.push_str(&format!("\npublished: {published}"));
    }
    if let Some(upvotes) = paper.get("upvotes").filter(|v| v.is_number()) {
        head.push_str(&format!("\nupvotes: {upvotes}"));
    }
    let abstract_text = text("summary");

    let links_block = |label: &str, total_key: &str, list: &Result<Vec<Linked>, String>| match list
    {
        Err(reason) => format!("\n{label} citing it: not readable ({reason})"),
        Ok(items) => {
            let total = paper.get(total_key).and_then(Value::as_u64);
            let count = match total {
                Some(total) => format!("{} shown of {total}", items.len()),
                None if items.len() as u32 >= PAPER_LINKS => {
                    format!("{} shown; there may be more", items.len())
                }
                None => format!("{} shown", items.len()),
            };
            let mut block = format!("\n{label} citing it ({count}, by downloads):");
            for (id, downloads) in items {
                match downloads {
                    Some(downloads) => block.push_str(&format!("\n{id} (downloads {downloads})")),
                    None => block.push_str(&format!("\n{id}")),
                }
            }
            block
        }
    };
    let links = format!(
        "{}{}",
        links_block("models", "numTotalModels", &models),
        links_block("datasets", "numTotalDatasets", &datasets)
    );

    let render = |limit: usize| {
        let mut page = Page::new(limit);
        page.push(&head, "header");
        if authors_cut {
            page.note(format!("authors beyond {MAX_AUTHORS} omitted"));
        }
        if let Some(abstract_text) = &abstract_text {
            page.push("\nabstract:\n", "abstract");
            let keep_for_links = links.len().min(page.room() / 2);
            let room = page.room().saturating_sub(keep_for_links);
            if abstract_text.len() > room {
                page.text.push_str(&cut(abstract_text, room));
                page.note("abstract cut to fit");
            } else {
                page.text.push_str(abstract_text);
            }
        }
        // `links` starts with '\n', so each piece is "\n<line>".
        let pieces: Vec<String> = links
            .split('\n')
            .skip(1)
            .map(|line| format!("\n{line}"))
            .collect();
        for (index, piece) in pieces.iter().enumerate() {
            if !page.try_push(piece) {
                page.note(format!(
                    "{} of {} citing-repo lines shown",
                    index,
                    pieces.len()
                ));
                break;
            }
        }
        page
    };
    let out_dir = ctx.out_dir;
    let rendered = finish(ctx.config.max_output_bytes, render, || {
        let full = json!({
            "paper": paper,
            "models": models_raw,
            "datasets": datasets_raw,
        });
        spill(out_dir, "paper", full.to_string().as_bytes())
    });
    read_success("paper", id, format!("paper {id}: {title}"), rendered)
}
