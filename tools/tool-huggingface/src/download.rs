//! `download`: one file, or one byte range of it, into the work folder.
//!
//! 1. A gateway `HEAD` of the resolve URL, following redirects with `HEAD` until a response
//!    reports `x-linked-size`, or `content-length` on a `200`. No reported size is
//!    `response_invalid`, and nothing is fetched.
//! 2. The bytes to fetch are checked against `max_download_bytes`; over it is `too_large`, with
//!    only `HEAD`s sent.
//! 3. A gateway `GET`, following the redirect rule in [`crate::http`]. A ranged fetch carries
//!    `Range` on every hop and must end in `206` with a matching `Content-Range`.
//! 4. The body streams into `<dest>.partial` while it is hashed, then is renamed to `dest`.
//!    Any over-run, under-run, status mismatch or sha256 mismatch removes the partial file.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Map};
use sha2::{Digest, Sha256};

use crate::http::{BodySink, ChainError, HttpFailure, Method, Route, USER_AGENT};
use crate::hub::resolve_path;
use crate::input::DownloadInput;
use crate::ops::{chain_failure, failure, http_failure, hub_failure, kind, Ctx, Response};
use crate::render::cut;

/// The suffix of the file a download streams into before it is complete.
pub const PARTIAL_SUFFIX: &str = ".partial";

/// Refuse a `dest` that already exists, whose partial file exists, or whose parent path runs
/// through a non-directory. Runs before any request.
pub fn check_dest(out_dir: &Path, dest: &str) -> Result<(), String> {
    let path = out_dir.join(dest);
    if fs::symlink_metadata(&path).is_ok() {
        return Err(format!(
            "\"dest\" {dest} already exists in the work folder; choose a new path (a download never replaces a file)"
        ));
    }
    if fs::symlink_metadata(partial_path(&path)).is_ok() {
        return Err(format!(
            "{dest}{PARTIAL_SUFFIX} already exists in the work folder; remove it or choose another \"dest\""
        ));
    }
    let mut parent = out_dir.to_path_buf();
    let components: Vec<&str> = dest.split('/').collect();
    for component in &components[..components.len() - 1] {
        parent.push(component);
        match fs::metadata(&parent) {
            Ok(meta) if !meta.is_dir() => {
                return Err(format!(
                    "\"dest\" {dest} runs through {}, which is a file, not a directory",
                    parent.strip_prefix(out_dir).unwrap_or(&parent).display()
                ))
            }
            _ => {}
        }
    }
    Ok(())
}

fn partial_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(PARTIAL_SUFFIX);
    PathBuf::from(name)
}

/// What the `HEAD` chain learned.
struct Head {
    size: u64,
    /// `x-linked-etag` when it is a sha256: 64 lowercase hex.
    sha256: Option<String>,
}

pub fn download(ctx: &mut Ctx, input: &DownloadInput) -> Response {
    let subject = format!(
        "{} {} file {} @ {}",
        input.kind.as_str(),
        input.repo,
        input.repo_file,
        input.revision
    );
    let url = ctx.hub(&resolve_path(
        input.kind,
        &input.repo,
        &input.revision,
        &input.repo_file,
    ));
    let user_agent = ("user-agent".to_string(), USER_AGENT.to_string());

    // 1. HEAD until a size is reported.
    let head = match ctx.client.fetch(
        Method::Head,
        &url,
        Route::Gateway,
        std::slice::from_ref(&user_agent),
        &|reply| reply.header("x-linked-size").is_some(),
    ) {
        Ok(fetched) => fetched.reply,
        Err(error) => return chain_failure(error),
    };
    let redirect_with_size =
        crate::http::is_redirect(head.status) && head.header("x-linked-size").is_some();
    if !(200..300).contains(&head.status) && !redirect_with_size {
        return hub_failure(head.status, &head.headers, &head.body, &subject, true);
    }
    let size = head
        .header("x-linked-size")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or_else(|| {
            (head.status == 200)
                .then(|| head.header("content-length"))
                .flatten()
                .and_then(|v| v.trim().parse::<u64>().ok())
        });
    let Some(size) = size else {
        return failure(
            kind::RESPONSE_INVALID,
            format!(
                "the Hub reported no size for {subject} (no x-linked-size, and no content-length on a 200); nothing was fetched"
            ),
            Map::new(),
        );
    };
    let sha256 = head
        .header("x-linked-etag")
        .map(|etag| {
            etag.trim()
                .trim_start_matches("W/")
                .trim_matches('"')
                .to_ascii_lowercase()
        })
        .filter(|etag| etag.len() == 64 && etag.bytes().all(|b| b.is_ascii_hexdigit()));
    let head = Head { size, sha256 };

    // 2. The bytes to fetch, against the cap.
    let (first, length) = match input.range {
        None => (0, head.size),
        Some((offset, _)) if offset >= head.size => {
            return failure(
                kind::INVALID_INPUT,
                format!(
                "\"byte_offset\" {offset} is at or past the end of {subject}, which is {} bytes",
                head.size
            ),
                Map::new(),
            )
        }
        Some((offset, length)) => (offset, length.min(head.size - offset)),
    };
    let cap = ctx.config.max_download_bytes;
    if length > cap {
        let mut extra = Map::new();
        extra.insert("size".to_string(), json!(length));
        extra.insert("cap".to_string(), json!(cap));
        let what = if input.range.is_some() {
            format!("the range of {length} bytes")
        } else {
            format!("{subject}, at {length} bytes,")
        };
        return failure(
            kind::TOO_LARGE,
            format!(
                "{what} is over this entry's max_download_bytes of {cap}; nothing was fetched \
                 (only HEAD requests were sent). Ask for a byte range with byte_offset and \
                 byte_length, or have the operator raise max_download_bytes"
            ),
            extra,
        );
    }

    // 3. GET, streaming into the partial file.
    let mut headers = vec![user_agent];
    let range = input.range.map(|_| (first, first + length - 1));
    if let Some((a, b)) = range {
        headers.push(("range".to_string(), format!("bytes={a}-{b}")));
    }
    let dest_path = ctx.out_dir.join(&input.dest);
    let partial = partial_path(&dest_path);
    let mut sink = Sink {
        partial: partial.clone(),
        file: None,
        expected: length,
        received: 0,
        hasher: Sha256::new(),
        range: range.map(|(a, b)| (a, b, head.size)),
        refused: None,
    };
    let fetched = ctx.client.stream(&url, Route::Gateway, &headers, &mut sink);
    let hasher = sink.hasher.clone();
    let received = sink.received;
    let opened = sink.file.is_some();
    let refused = sink.refused.take();
    drop(sink);
    let cleanup = || {
        let _ = fs::remove_file(&partial);
    };
    let fetched = match fetched {
        Ok(fetched) => fetched,
        Err(ChainError::Http {
            failure: HttpFailure::Aborted(message),
            ..
        }) => {
            cleanup();
            let error_kind = if message.starts_with("more bytes") {
                kind::RESPONSE_INVALID
            } else {
                kind::WRITE_FAILED
            };
            return failure(error_kind, message, Map::new());
        }
        Err(ChainError::Http {
            failure: http,
            host,
        }) => {
            cleanup();
            return http_failure(http, &host);
        }
        Err(error) => {
            cleanup();
            return chain_failure(error);
        }
    };
    if let Some((error_kind, message)) = refused {
        cleanup();
        return failure(error_kind, message, Map::new());
    }
    let reply = &fetched.reply;
    if !(200..300).contains(&reply.status) {
        cleanup();
        return hub_failure(reply.status, &reply.headers, &reply.body, &subject, true);
    }
    if !opened {
        cleanup();
        return failure(
            kind::RESPONSE_INVALID,
            format!("the final response for {subject} carried no body to save"),
            Map::new(),
        );
    }
    if received != length {
        cleanup();
        return failure(
            kind::RESPONSE_INVALID,
            format!(
                "expected {length} bytes of {subject} but the response ended after {received}; \
                 the partial file was removed"
            ),
            Map::new(),
        );
    }
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let mut verified = false;
    if range.is_none() {
        if let Some(expected) = &head.sha256 {
            if *expected != digest {
                cleanup();
                return failure(
                    kind::RESPONSE_INVALID,
                    format!(
                        "{subject} hashed to sha256 {digest}, not the {expected} the Hub reported \
                         in x-linked-etag; the file was removed"
                    ),
                    Map::new(),
                );
            }
            verified = true;
        }
    }
    // 4. Rename into place, never over an existing file.
    if fs::symlink_metadata(&dest_path).is_ok() {
        cleanup();
        return failure(
            kind::WRITE_FAILED,
            format!(
                "{} appeared while the download ran; it was not replaced",
                input.dest
            ),
            Map::new(),
        );
    }
    if let Err(e) = fs::rename(&partial, &dest_path) {
        cleanup();
        return failure(
            kind::WRITE_FAILED,
            format!("moving the download into {} failed: {e}", input.dest),
            Map::new(),
        );
    }

    let mut text = format!(
        "Hugging Face download: {} {} {} @ {}",
        input.kind.as_str(),
        input.repo,
        input.repo_file,
        input.revision
    );
    text.push_str(ctx.defaults_line());
    text.push_str(&format!("\ndest: {}", input.dest));
    match range {
        Some((a, b)) => text.push_str(&format!(
            "\nbytes: {length} (range {a}–{b} of {})",
            head.size
        )),
        None => text.push_str(&format!("\nbytes: {length} (whole file)")),
    }
    text.push_str(&format!("\nsha256: {digest}"));
    if verified {
        text.push_str(" (matches the Hub's x-linked-etag)");
    }
    let hops: Vec<String> = fetched.hops.iter().map(|hop| hop.describe()).collect();
    text.push_str(&format!("\nfetched: {}", hops.join(" → ")));
    let text = cut(&text, ctx.config.max_output_bytes);
    Response {
        status: crate::ops::OpStatus::Passed,
        summary: format!("downloaded {} ({length} bytes)", input.dest),
        data: text,
        data_path: None,
        truncated: false,
        metadata: vec![
            ("state_effect".to_string(), "mutate".to_string()),
            ("resource_id".to_string(), input.dest.clone()),
        ],
    }
}

/// Streams the final `2xx` body into the partial file, counting and hashing it.
struct Sink {
    partial: PathBuf,
    file: Option<File>,
    expected: u64,
    received: u64,
    hasher: Sha256,
    /// `(first, last, size)` of a ranged fetch.
    range: Option<(u64, u64, u64)>,
    /// Why the final response was refused before any byte was read.
    refused: Option<(&'static str, String)>,
}

impl BodySink for Sink {
    fn accept(&mut self, status: u16, headers: &[(String, String)]) -> bool {
        let header = |name: &str| {
            headers
                .iter()
                .find(|(candidate, _)| candidate == name)
                .map(|(_, value)| value.as_str())
        };
        match self.range {
            None if status != 200 => {
                self.refused = Some((
                    kind::RESPONSE_INVALID,
                    format!("a whole-file download must end in HTTP 200, got {status}"),
                ));
                return false;
            }
            Some(_) if status != 206 => {
                self.refused = Some((
                    kind::RESPONSE_INVALID,
                    format!(
                        "a ranged download must end in HTTP 206 Partial Content, got {status}; \
                         the server ignored the Range header"
                    ),
                ));
                return false;
            }
            Some((first, last, size)) => {
                let content_range = header("content-range").unwrap_or_default();
                if !content_range_matches(content_range, first, last, size) {
                    self.refused = Some((
                        kind::RESPONSE_INVALID,
                        format!(
                            "the 206 answered Content-Range '{content_range}', not bytes \
                             {first}-{last}/{size}"
                        ),
                    ));
                    return false;
                }
            }
            None => {}
        }
        if let Some(parent) = self.partial.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                self.refused = Some((
                    kind::WRITE_FAILED,
                    format!("creating the destination folder failed: {e}"),
                ));
                return false;
            }
        }
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.partial)
        {
            Ok(file) => {
                self.file = Some(file);
                true
            }
            Err(e) => {
                let why = if e.kind() == ErrorKind::AlreadyExists {
                    "it already exists".to_string()
                } else {
                    e.to_string()
                };
                self.refused = Some((
                    kind::WRITE_FAILED,
                    format!("opening the partial file failed: {why}"),
                ));
                false
            }
        }
    }

    fn write(&mut self, chunk: &[u8]) -> Result<(), String> {
        self.received += chunk.len() as u64;
        if self.received > self.expected {
            return Err(format!(
                "more bytes arrived than the {} expected; the partial file was removed",
                self.expected
            ));
        }
        self.hasher.update(chunk);
        self.file
            .as_mut()
            .ok_or_else(|| "no partial file is open".to_string())?
            .write_all(chunk)
            .map_err(|e| format!("writing the partial file failed: {e}"))
    }
}

/// `bytes <first>-<last>/<size or *>` naming exactly the asked range.
fn content_range_matches(value: &str, first: u64, last: u64, size: u64) -> bool {
    let Some(rest) = value.trim().strip_prefix("bytes ") else {
        return false;
    };
    let Some((span, total)) = rest.split_once('/') else {
        return false;
    };
    let Some((a, b)) = span.split_once('-') else {
        return false;
    };
    let total_ok = total.trim() == "*" || total.trim().parse::<u64>() == Ok(size);
    a.trim().parse::<u64>() == Ok(first) && b.trim().parse::<u64>() == Ok(last) && total_ok
}
