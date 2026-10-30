//! The request-body writer for a guest that sends a `wasi:http` body.
//!
//! The host buffers an outgoing body in a bounded channel, and only the send that
//! `outgoing-handler.handle` starts drains it. A guest that writes the whole body before
//! calling `handle` therefore blocks forever once the body outgrows that buffer. The caller's
//! contract, in order:
//!
//! 1. take the body with `outgoing-request.body`;
//! 2. dispatch the request with `outgoing-handler.handle`;
//! 3. stream the body with [`write_body`];
//! 4. on `Ok`, `outgoing-body.finish`; on [`BodyWrite::Closed`], drop the body and let the
//!    response future report why the host stopped reading; on [`BodyWrite::Failed`], return
//!    the message;
//! 5. await the response future.
//!
//! The `Failed` messages carry no prefix; a caller adds its own.
#![cfg(target_arch = "wasm32")]

/// Why a request body was not written whole.
pub enum BodyWrite {
    /// The host closed the body stream.
    Closed,
    /// Any other failure, as its error message.
    Failed(String),
}

/// Stream `body` into `outgoing_body`, after `outgoing-handler.handle`: nothing drains the
/// host's bounded body buffer before then. The output stream is a child of `outgoing_body`
/// and drops when this returns, before the caller finishes or drops its parent.
pub fn write_body(
    outgoing_body: &wasip2::http::types::OutgoingBody,
    body: &[u8],
) -> Result<(), BodyWrite> {
    let stream = outgoing_body
        .write()
        .map_err(|()| BodyWrite::Failed("failed to open request body stream".to_string()))?;
    let mut remaining: &[u8] = body;
    while !remaining.is_empty() {
        let budget = stream
            .check_write()
            .map_err(|e| body_write(e, |e| format!("check-write failed: {e:?}")))?
            as usize;
        if budget == 0 {
            stream.subscribe().block();
            continue;
        }
        let n = budget.min(remaining.len());
        stream
            .write(&remaining[..n])
            .map_err(|e| body_write(e, |e| format!("write failed: {e:?}")))?;
        remaining = &remaining[n..];
    }
    stream
        .flush()
        .map_err(|e| body_write(e, |e| format!("flush failed: {e:?}")))?;
    stream.subscribe().block();
    Ok(())
}

/// [`BodyWrite::Closed`] for a closed stream, otherwise `failed`'s message.
fn body_write(
    err: wasip2::io::streams::StreamError,
    failed: fn(wasip2::io::streams::StreamError) -> String,
) -> BodyWrite {
    match err {
        wasip2::io::streams::StreamError::Closed => BodyWrite::Closed,
        err => BodyWrite::Failed(failed(err)),
    }
}
