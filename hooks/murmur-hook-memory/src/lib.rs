//! Memory hook: seed a task with the part of the conversation that came before it.
//!
//! The runtime keeps one durable record per conversation and is its only writer. This
//! hook is a reader: bound to `on-task-start` with `commit_policy: seed-context`, it pages
//! that record through `murmur:conversation/read`, picks a chronological slice that fits
//! the task's seed budget, and hands it back as the head of the new task's context. It
//! holds no `filesystem` grant, opens no file, and writes nothing anywhere.
//!
//! Each seeded message carries `source-id` set to the `msg_` id of the record message it
//! was rendered from, and `id: none` so the runtime mints a fresh identity for the copy —
//! an id must never repeat, so reusing the record's id on a new message would be wrong.
//! `source-id` is the join key back to the record: it is what lets a `context_seed` trace
//! line be traced to the turns it came from.
//!
//! Split the way `murmur-hook-compact` is: a `cfg`-independent [`recall`] module holding
//! the whole control flow (paging termination, rendering, selection, budget accounting,
//! ordering) with every impure step injected as a closure, and a `wasm_hook` module
//! compiled only for `wasm32` that supplies the real host imports as those closures.

// ── Pure, host-testable logic (no WASM bindings, no `cfg`) ────────────────────
pub mod recall {
    use serde_json::Value;

    pub use murmur_hook_transcript::{extract_text, unwrap_tool_envelope, TOOL_MARKER};

    /// Page size asked of `read-messages`. The host clamps `limit` to `1..=100`, so asking
    /// for more buys nothing and asking for less only costs round trips.
    pub const PAGE_LIMIT: u32 = 100;

    /// Ceiling on record messages walked in one `on-task-start`. The record grows for the
    /// life of a conversation while the seed that can be built from it is bounded by
    /// `budget-tokens`, so paging the whole of a long one is work whose result is thrown
    /// away. The walk starts at the newest message, so the cap drops the oldest history.
    pub const MAX_SCANNED_MESSAGES: usize = 2_000;

    /// Ceiling on pages fetched, independent of [`MAX_SCANNED_MESSAGES`]. A page that
    /// hands back a cursor while returning no messages would otherwise loop forever, and
    /// this hook is `execution_mode: blocking` — the task waits on it.
    pub const MAX_PAGES: usize = 64;

    /// Ceiling on seeded messages, whatever the budget allows. A seed of hundreds of tiny
    /// messages costs the model more attention than it repays, and every one of them is
    /// context the task itself does not get to use.
    pub const MAX_SEEDED_MESSAGES: usize = 40;

    /// Bytes of task text read for relevance scoring. Terms repeat long before this, so a
    /// wider window changes the scores hardly at all while copying the whole input into
    /// the hook's memory.
    pub const TASK_TEXT_WINDOW: u64 = 8 * 1024;

    /// Percent of `budget-tokens` held back from the seed. The host measures the committed
    /// seed by re-serializing each message itself, and its key order and separators need
    /// not match [`wire_form`] byte for byte. The margin is what keeps a proposal that
    /// measures at the ceiling here from arriving a few tokens over it there, which would
    /// turn a `seeded` outcome into a `trimmed` one.
    pub const BUDGET_HEADROOM_PERCENT: u64 = 5;

    /// The exact error text `read-messages` returns when this hook's entry does not
    /// declare `capabilities.conversation.read: true`.
    pub const NOT_GRANTED: &str = "not-granted";

    /// The artifact name every error this hook raises names, so a line in
    /// `workdir/logs/hook-murmur-hook-memory.log` is attributable without cross-referencing
    /// the capsule manifest.
    pub const ARTIFACT_NAME: &str = "murmur-hook-memory";

    /// One message as the conversation record holds it, independent of the WIT bindings so
    /// it can be built and asserted on in host tests.
    ///
    /// `source_id` is the record line's own join key, which is what [`drop_reseeded`]
    /// reads to tell an earlier seed's copy apart from the message it was copied from.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct RecordMessage {
        pub role: String,
        pub content: String,
        pub id: Option<String>,
        pub source_id: Option<String>,
    }

    /// One page of the record, newest first — the bindings-free form of the WIT
    /// `message-page`. `total` is deliberately absent: it is a snapshot that grows while
    /// the runtime appends, so a paging loop that consulted it would be wrong.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct RecordPage {
        pub messages: Vec<RecordMessage>,
        pub next_cursor: Option<String>,
    }

    /// One message this hook proposes as seed context. `source_id` is the record message's
    /// `id`; the seeded message's own `id` is minted by the runtime and is never set here.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct SeedMessage {
        pub role: String,
        pub content: String,
        pub source_id: Option<String>,
    }

    /// Lowercase `[a-z0-9]+` runs. Query and message tokenize by this one rule, so a term
    /// matches on both sides or neither. Anything else — punctuation, whitespace, any
    /// non-ASCII character — separates.
    pub fn terms(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut current = String::new();
        for ch in text.chars() {
            let lowered = ch.to_ascii_lowercase();
            if lowered.is_ascii_alphanumeric() {
                current.push(lowered);
            } else if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        }
        if !current.is_empty() {
            out.push(current);
        }
        out
    }

    /// Render one record message into a seedable one, or `None` for a message that must
    /// not be seeded.
    ///
    /// `system` is dropped: the capsule's system prompt is sent on every request anyway, so
    /// re-seeding it only spends budget. A message that renders to nothing but whitespace
    /// is dropped for the same reason.
    ///
    /// A `"tool"`-role message is unwrapped out of its envelope and re-roled to `user`,
    /// because the runtime drops a returned `tool` message that no longer carries an intact
    /// envelope, and an unpaired tool result at the head of a context is a driver error
    /// even when kept. Everything that is not an `assistant` turn becomes `user`, so the
    /// seed only ever holds the two roles every driver accepts.
    pub fn render(message: &RecordMessage) -> Option<SeedMessage> {
        if message.role == "system" {
            return None;
        }

        let text = if message.role == "tool" {
            unwrap_tool_envelope(&message.content)
                .unwrap_or_else(|| extract_text(&message.content))
        } else {
            extract_text(&message.content)
        };

        if text.trim().is_empty() {
            return None;
        }

        let role = if message.role == "assistant" {
            "assistant"
        } else {
            "user"
        };

        // JSON-encoded, because the runtime parses a returned message's content back out of
        // JSON and only falls back to a raw text block when that fails. Encoding means text
        // that happens to look like JSON (`42`, `[...]`) still arrives as text.
        Some(SeedMessage {
            role: role.to_string(),
            content: serde_json::to_string(&text).unwrap_or_else(|_| text.clone()),
            source_id: message.id.clone(),
        })
    }

    /// The JSON the runtime reconstructs this message into before measuring it — a
    /// `Value::String` content becomes a single text block. Token accounting has to be done
    /// over this shape rather than over the bare content, or every message is
    /// under-measured by the weight of its own envelope.
    pub fn wire_form(message: &SeedMessage) -> String {
        let text = plain_text(&message.content);
        let role = serde_json::to_string(&message.role).unwrap_or_else(|_| "\"user\"".to_string());
        let text = serde_json::to_string(&text).unwrap_or_else(|_| "\"\"".to_string());
        format!(r#"{{"role":{role},"content":[{{"type":"text","text":{text}}}]}}"#)
    }

    /// The budget a seed is actually built against: `budget_tokens` less
    /// [`BUDGET_HEADROOM_PERCENT`].
    pub fn effective_budget(budget_tokens: u64) -> u64 {
        // Widened, because `budget_tokens * 5` overflows a u64 near its ceiling while
        // `budget_tokens / 100 * 5` rounds the margin away entirely on a small budget.
        let margin = (budget_tokens as u128 * BUDGET_HEADROOM_PERCENT as u128 / 100) as u64;
        budget_tokens.saturating_sub(margin)
    }

    /// Why this task must not be seeded, or `None` to go ahead.
    ///
    /// Both cases are ordinary and neither is an error: the hook returns `hook-output.none`
    /// and the runtime writes no `context_seed` line at all.
    pub fn should_decline(budget_tokens: u64, prior_tokens: u64) -> Option<&'static str> {
        if prior_tokens > 0 {
            // `lifecycle.conversation: threaded` — the host already reloaded this
            // conversation's history, and seeding on top of it duplicates it.
            return Some("the host already loaded prior context for this task");
        }
        if budget_tokens == 0 {
            // `0` is "not computed", never "unbounded": a capsule declaring no
            // `context.max_tokens` sends it, and refuses any seed proposed against it.
            return Some("the capsule computed no seed budget");
        }
        None
    }

    /// Walk the record newest-first and return it chronologically, oldest first.
    ///
    /// `read_page` is handed `None` for the first page and thereafter the previous page's
    /// `next-cursor` unmodified. The walk ends on `next-cursor: none`, which is the only
    /// correct termination condition — a page's `total` is a snapshot that can grow while
    /// the runtime appends, so a loop sized by counting up to it would stop early or never.
    /// [`MAX_SCANNED_MESSAGES`] and [`MAX_PAGES`] bound it besides.
    ///
    /// A record that does not exist is not an error: it is an empty first page, and this
    /// returns an empty vector. That covers a first-ever run, `context.record: off`, and
    /// any capsule running `inference.transport: process`.
    pub fn collect_record<F>(mut read_page: F) -> Result<Vec<RecordMessage>, String>
    where
        F: FnMut(Option<String>) -> Result<RecordPage, String>,
    {
        let mut newest_first: Vec<RecordMessage> = Vec::new();
        let mut cursor: Option<String> = None;

        for _ in 0..MAX_PAGES {
            let page = read_page(cursor.take()).map_err(read_failure)?;
            newest_first.extend(page.messages);

            match page.next_cursor {
                Some(next) if newest_first.len() < MAX_SCANNED_MESSAGES => cursor = Some(next),
                _ => break,
            }
        }

        newest_first.reverse();
        Ok(newest_first)
    }

    /// Turn a `read-messages` error string into one that names this artifact.
    ///
    /// `not-granted` gets the full remedy, because there is no staging-time check for the
    /// grant: an ungranted hook links, runs, and reads nothing, which is byte-identical to
    /// a capsule whose record is genuinely empty. Failing loudly here is what tells those
    /// two apart, in `workdir/logs/hook-murmur-hook-memory.log`.
    pub fn read_failure(error: String) -> String {
        if error.trim() == NOT_GRANTED {
            return format!(
                "{ARTIFACT_NAME}: reading the conversation record returned `{NOT_GRANTED}`, so \
                 no context can be seeded. Add `capabilities.conversation.read: true` to this \
                 hook's own entry under `artifacts:` in the capsule's murmur.yaml. The grant is \
                 per-hook and default-deny; a capabilities block inside the hook artifact is \
                 inert, and one in the capsule-wide `capabilities:` block reaches nothing."
            );
        }
        format!("{ARTIFACT_NAME}: reading the conversation record failed: {error}")
    }

    /// Drop the copies an earlier seed left behind in the record.
    ///
    /// A committed seed is appended to the record like any other message, carrying
    /// `source-id` set to the message it was copied from. Left in, the next task seeds the
    /// original and the copy both, and the task after that seeds all four — the seed
    /// doubles per run until the budget caps it, and the model is shown the same turn over
    /// and over in place of history it has not seen yet.
    ///
    /// A message whose `source-id` names another message in this same walk is exactly such
    /// a copy, and the message it names is still here to be seeded in its place. One whose
    /// `source-id` names something outside the walk — a corpus record, a compaction
    /// summary, or an original the scan cap cut off — is content in its own right and is
    /// kept.
    pub fn drop_reseeded(record: Vec<RecordMessage>) -> Vec<RecordMessage> {
        let ids: std::collections::HashSet<&str> = record
            .iter()
            .filter_map(|m| m.id.as_deref())
            .collect();
        let reseeded: Vec<bool> = record
            .iter()
            .map(|m| {
                m.source_id
                    .as_deref()
                    .is_some_and(|source| ids.contains(source))
            })
            .collect();
        record
            .into_iter()
            .zip(reseeded)
            .filter_map(|(message, is_copy)| (!is_copy).then_some(message))
            .collect()
    }

    /// Choose which candidates to seed, chronological in and chronological out.
    ///
    /// With `task_text`, each candidate scores as the fraction of the task's distinct terms
    /// it contains, and candidates are taken score-first, newest-first within a tie. A task
    /// that tokenizes to nothing scores every candidate `0`, which collapses the order to
    /// pure recency — the same order taken when there is no task text at all.
    ///
    /// `count` measures each candidate's [`wire_form`], and a candidate is admitted only
    /// while the running total stays inside [`effective_budget`]. One oversized message
    /// does not end the walk: a later, smaller one still gets its chance.
    pub fn select<F>(
        candidates: Vec<SeedMessage>,
        task_text: Option<&str>,
        budget_tokens: u64,
        mut count: F,
    ) -> Vec<SeedMessage>
    where
        F: FnMut(&str) -> u64,
    {
        let budget = effective_budget(budget_tokens);
        if budget == 0 || candidates.is_empty() {
            return Vec::new();
        }

        let query = distinct_terms(task_text.unwrap_or_default());
        let scores: Vec<f64> = candidates.iter().map(|c| overlap(c, &query)).collect();

        let mut order: Vec<usize> = (0..candidates.len()).collect();
        order.sort_by(|&a, &b| {
            scores[b]
                .partial_cmp(&scores[a])
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.cmp(&a))
        });

        let mut admitted: Vec<usize> = Vec::new();
        let mut spent: u64 = 0;
        for index in order {
            if admitted.len() >= MAX_SEEDED_MESSAGES {
                break;
            }
            let cost = count(&wire_form(&candidates[index]));
            // Saturating, because `count` is the host's answer rather than this hook's: a
            // count near `u64::MAX` would overflow the sum and admit the message instead
            // of rejecting it.
            if spent.saturating_add(cost) > budget {
                continue;
            }
            spent += cost;
            admitted.push(index);
        }

        // Back into record order: `seed-context` is chronological, oldest first, which is
        // what makes the runtime's "drop from the front" trim well-defined.
        admitted.sort_unstable();
        let mut kept: Vec<Option<SeedMessage>> = candidates.into_iter().map(Some).collect();
        admitted
            .into_iter()
            .filter_map(|index| kept[index].take())
            .collect()
    }

    /// The whole `on-task-start` decision, with every host call injected.
    ///
    /// The order matters and is asserted on: the decline checks run before `read_page` and
    /// before `task_text` are ever called, so a task that cannot be seeded costs no host
    /// call at all. An empty return means "seed nothing" and is not an error.
    pub fn plan_seed<R, T, C>(
        budget_tokens: u64,
        prior_tokens: u64,
        read_page: R,
        task_text: T,
        count: C,
    ) -> Result<Vec<SeedMessage>, String>
    where
        R: FnMut(Option<String>) -> Result<RecordPage, String>,
        T: FnOnce() -> Option<String>,
        C: FnMut(&str) -> u64,
    {
        if should_decline(budget_tokens, prior_tokens).is_some() {
            return Ok(Vec::new());
        }

        let record = drop_reseeded(collect_record(read_page)?);
        if record.is_empty() {
            return Ok(Vec::new());
        }

        let candidates: Vec<SeedMessage> = record.iter().filter_map(render).collect();
        let task = task_text();
        Ok(select(candidates, task.as_deref(), budget_tokens, count))
    }

    /// A seed message's content back as plain text. Content is JSON-encoded by [`render`],
    /// so this is the inverse; anything that is not JSON is taken verbatim.
    fn plain_text(content: &str) -> String {
        match serde_json::from_str::<Value>(content) {
            Ok(Value::String(s)) => s,
            _ => content.to_string(),
        }
    }

    fn distinct_terms(text: &str) -> Vec<String> {
        let mut out = terms(text);
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Fraction of `query`'s distinct terms present in the message. `0.0` for an empty
    /// query, so a task that tokenizes to nothing scores nothing rather than everything.
    fn overlap(message: &SeedMessage, query: &[String]) -> f64 {
        if query.is_empty() {
            return 0.0;
        }
        let present = distinct_terms(&plain_text(&message.content));
        let hits = query.iter().filter(|t| present.binary_search(t).is_ok()).count();
        hits as f64 / query.len() as f64
    }
}

// ── WASM adapter: WIT bindings ↔ pure logic (wasm32 only) ─────────────────────
#[cfg(target_arch = "wasm32")]
mod wasm_hook {
    use crate::recall::{self, RecordMessage, RecordPage, SeedMessage};

    wit_bindgen::generate!({
        path: "../../wit/hook",
        world: "hook",
        generate_all,
    });

    use murmur::conversation::read::read_messages;
    use murmur::runtime::tokens::count;
    use murmur::task_io::read::{input_len, read_input, TaskInputForm};

    use exports::murmur::hook::lifecycle::{
        CompactionEvent, Guest, HookOutput, InferenceEvent, Message, SessionContext,
        SessionEndEvent, ShellEvent, StageEvent, TaskEndEvent, TaskStartEvent, ToolEvent,
    };

    pub struct MurmurMemory;

    impl Guest for MurmurMemory {
        fn on_task_start(event: TaskStartEvent) -> Result<HookOutput, String> {
            let seeds = recall::plan_seed(
                event.budget_tokens,
                event.prior_tokens,
                fetch_page,
                task_text,
                count,
            )?;

            if seeds.is_empty() {
                return Ok(HookOutput::None);
            }

            Ok(HookOutput::SeedContext(seeds.into_iter().map(seed).collect()))
        }

        fn on_stage(_event: StageEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_session_start(_ctx: SessionContext) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_inference(_event: InferenceEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_tool_call(_event: ToolEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_shell(_event: ShellEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_compaction(_event: CompactionEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_task_end(_event: TaskEndEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }

        fn on_session_end(_event: SessionEndEvent) -> Result<HookOutput, String> {
            Ok(HookOutput::None)
        }
    }

    /// One page of the record. The `message` a page hands back is the *import*-side
    /// `murmur::hook::lifecycle::Message`, a distinct type from the export-side `Message`
    /// [`seed`] builds, so the conversion is explicit in both directions. A record
    /// message's `source-id` is carried through, because it is what marks the line as a
    /// copy an earlier seed left behind — see `recall::drop_reseeded`.
    fn fetch_page(cursor: Option<String>) -> Result<RecordPage, String> {
        let page = read_messages(cursor.as_deref(), recall::PAGE_LIMIT)?;
        Ok(RecordPage {
            messages: page
                .messages
                .into_iter()
                .map(|m| RecordMessage {
                    role: m.role,
                    content: m.content,
                    id: m.id,
                    source_id: m.source_id,
                })
                .collect(),
            next_cursor: page.next_cursor,
        })
    }

    /// The task's own text, for relevance scoring, or `None` to fall back to recency.
    ///
    /// At `on-task-start` the task is not yet in scope, so today this returns `no-task` on
    /// every capsule. `not-granted` and `no-task` are both ordinary fallback signals, never
    /// errors — the read is attempted regardless so the relevance path lights up unchanged
    /// if a future runtime brings the task into scope here.
    fn task_text() -> Option<String> {
        let len = input_len(TaskInputForm::AsGiven).ok()?;
        if len == 0 {
            return None;
        }
        let text = read_input(
            TaskInputForm::AsGiven,
            0,
            len.min(recall::TASK_TEXT_WINDOW),
        )
        .ok()?;
        if text.trim().is_empty() {
            None
        } else {
            Some(text)
        }
    }

    /// `id: None` so the runtime mints a fresh identity — an id must never repeat, so
    /// carrying the record's id onto a copy of it would be wrong. `source_id` carries that
    /// record id instead, verbatim, as the join key back.
    fn seed(message: SeedMessage) -> Message {
        Message {
            role: message.role,
            content: message.content,
            id: None,
            source_id: message.source_id,
        }
    }

    export!(MurmurMemory);
}

// ── Host-runnable unit tests for the pure recall logic ────────────────────────
#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::recall::{
        collect_record, drop_reseeded, effective_budget, plan_seed, render, select,
        should_decline, terms, unwrap_tool_envelope, wire_form, RecordMessage, RecordPage,
        SeedMessage, MAX_SEEDED_MESSAGES, NOT_GRANTED, TOOL_MARKER,
    };

    fn record(role: &str, content: &str, id: &str) -> RecordMessage {
        RecordMessage {
            role: role.to_string(),
            content: serde_json::to_string(content).unwrap(),
            id: Some(id.to_string()),
            source_id: None,
        }
    }

    /// A record line an earlier seed left behind: a copy of `source`, with an id of its own.
    fn reseeded(role: &str, content: &str, id: &str, source: &str) -> RecordMessage {
        RecordMessage {
            source_id: Some(source.to_string()),
            ..record(role, content, id)
        }
    }

    fn page(messages: Vec<RecordMessage>, next: Option<&str>) -> RecordPage {
        RecordPage {
            messages,
            next_cursor: next.map(str::to_string),
        }
    }

    /// Replays a scripted list of pages and records the cursor it was handed each time.
    struct FakeRecord {
        pages: RefCell<Vec<Result<RecordPage, String>>>,
        cursors: RefCell<Vec<Option<String>>>,
    }

    impl FakeRecord {
        fn new(pages: Vec<Result<RecordPage, String>>) -> Self {
            FakeRecord {
                pages: RefCell::new(pages),
                cursors: RefCell::new(Vec::new()),
            }
        }

        fn read(&self, cursor: Option<String>) -> Result<RecordPage, String> {
            self.cursors.borrow_mut().push(cursor);
            self.pages.borrow_mut().remove(0)
        }
    }

    /// One token per whitespace-separated word — enough to make budget arithmetic in a test
    /// predictable without asserting on the host's real `cl100k_base` numbers.
    fn word_count(text: &str) -> u64 {
        text.split_whitespace().count() as u64
    }

    // ── decline checks ────────────────────────────────────────────────────────

    #[test]
    fn threaded_conversation_and_missing_budget_both_decline() {
        assert!(should_decline(4_000, 512).is_some(), "prior tokens present");
        assert!(should_decline(0, 0).is_some(), "no budget computed");
        assert!(should_decline(0, 512).is_some(), "both at once");
        assert!(should_decline(4_000, 0).is_none(), "the ordinary case");
    }

    #[test]
    fn declining_seeds_nothing_and_makes_no_host_call() {
        for (budget, prior) in [(0u64, 0u64), (4_000, 128)] {
            let mut pages_read = 0;
            let mut task_reads = 0;
            let out = plan_seed(
                budget,
                prior,
                |_| {
                    pages_read += 1;
                    Ok(page(Vec::new(), None))
                },
                || {
                    task_reads += 1;
                    None
                },
                word_count,
            )
            .expect("declining is not an error");

            assert!(out.is_empty());
            assert_eq!(pages_read, 0, "the record must not be read at all");
            assert_eq!(task_reads, 0, "the task text must not be read at all");
        }
    }

    // ── paging ────────────────────────────────────────────────────────────────

    #[test]
    fn paging_terminates_on_next_cursor_none_not_on_a_growing_total() {
        // Three pages; the record is still being appended to, so a loop sized by any
        // running count would stop at the wrong place. Only `next-cursor: none` ends it.
        let fake = FakeRecord::new(vec![
            Ok(page(vec![record("user", "c", "msg_c")], Some("cur1"))),
            Ok(page(vec![record("user", "b", "msg_b")], Some("cur2"))),
            Ok(page(vec![record("user", "a", "msg_a")], None)),
        ]);

        let out = collect_record(|c| fake.read(c)).expect("walk succeeded");

        assert_eq!(
            fake.cursors.into_inner(),
            vec![None, Some("cur1".to_string()), Some("cur2".to_string())],
            "the first read starts at the newest message; each later one passes the \
             previous next-cursor back unmodified"
        );
        // Newest-first pages, reassembled oldest first.
        let ids: Vec<_> = out.iter().map(|m| m.id.clone().unwrap()).collect();
        assert_eq!(ids, vec!["msg_a", "msg_b", "msg_c"]);
    }

    #[test]
    fn an_empty_record_is_an_empty_walk_and_not_an_error() {
        let fake = FakeRecord::new(vec![Ok(page(Vec::new(), None))]);
        assert_eq!(collect_record(|c| fake.read(c)).unwrap(), Vec::new());
    }

    #[test]
    fn not_granted_names_the_artifact_the_key_and_where_it_belongs() {
        let fake = FakeRecord::new(vec![Err(NOT_GRANTED.to_string())]);
        let err = collect_record(|c| fake.read(c)).unwrap_err();

        assert!(err.contains("murmur-hook-memory"), "{err}");
        assert!(err.contains("capabilities.conversation.read: true"), "{err}");
        assert!(err.contains("murmur.yaml"), "{err}");
    }

    #[test]
    fn any_other_read_failure_carries_the_hosts_text() {
        let fake = FakeRecord::new(vec![Err("unavailable: disk gone".to_string())]);
        let err = collect_record(|c| fake.read(c)).unwrap_err();

        assert!(err.contains("murmur-hook-memory"), "{err}");
        assert!(err.contains("unavailable: disk gone"), "{err}");
        assert!(
            !err.contains("capabilities.conversation.read"),
            "a non-grant failure must not send the operator after a grant it already has"
        );
    }

    // ── rendering ─────────────────────────────────────────────────────────────

    fn envelope(id: serde_json::Value, is_error: serde_json::Value, body: serde_json::Value) -> String {
        serde_json::json!({
            TOOL_MARKER: true,
            "tool_call_id": id,
            "is_error": is_error,
            "body": body,
        })
        .to_string()
    }

    #[test]
    fn tool_envelopes_unwrap_into_readable_user_text() {
        let wrapped = envelope(
            serde_json::json!("call_42"),
            serde_json::Value::Null,
            serde_json::json!("3 tests passed"),
        );
        let rendered = render(&RecordMessage {
            role: "tool".to_string(),
            content: wrapped,
            id: Some("msg_t".to_string()),
            source_id: None,
        })
        .expect("a tool result seeds");

        assert_eq!(rendered.role, "user", "a tool-role message is always dropped");
        assert!(rendered.content.contains("call_42"), "{}", rendered.content);
        assert!(rendered.content.contains("3 tests passed"), "{}", rendered.content);
        assert!(!rendered.content.contains(TOOL_MARKER), "no raw envelope JSON is seeded");
        assert_eq!(rendered.source_id.as_deref(), Some("msg_t"));
    }

    #[test]
    fn an_error_envelope_is_flagged_and_a_missing_call_id_still_renders() {
        let failed = envelope(
            serde_json::json!("call_7"),
            serde_json::json!(true),
            serde_json::json!([{"type": "text", "text": "exit code 1"}]),
        );
        let rendered = render(&RecordMessage {
            role: "tool".to_string(),
            content: failed,
            id: None,
            source_id: None,
        })
        .unwrap();
        assert!(rendered.content.contains("(error)"), "{}", rendered.content);
        assert!(rendered.content.contains("exit code 1"), "{}", rendered.content);
        assert_eq!(rendered.source_id, None, "a record message with no id still seeds");

        let bare = envelope(
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::json!("output"),
        );
        let rendered = render(&RecordMessage {
            role: "tool".to_string(),
            content: bare,
            id: None,
            source_id: None,
        })
        .unwrap();
        assert!(rendered.content.contains("unknown"), "{}", rendered.content);
    }

    #[test]
    fn content_that_is_json_but_not_an_envelope_and_content_that_is_not_json_at_all() {
        assert_eq!(unwrap_tool_envelope(r#"{"__murmur_tool_msg__":false,"body":"x"}"#), None);
        assert_eq!(unwrap_tool_envelope(r#"{"body":"x"}"#), None);
        assert_eq!(unwrap_tool_envelope("just some output"), None);
        assert_eq!(unwrap_tool_envelope("[1,2,3]"), None);

        // Both fall through to plain text handling rather than seeding raw JSON.
        let json_not_envelope = render(&RecordMessage {
            role: "tool".to_string(),
            content: r#"{"body":"x"}"#.to_string(),
            id: None,
            source_id: None,
        })
        .unwrap();
        assert_eq!(json_not_envelope.content, r#""{\"body\":\"x\"}""#);

        let not_json = render(&RecordMessage {
            role: "tool".to_string(),
            content: "raw output".to_string(),
            id: None,
            source_id: None,
        })
        .unwrap();
        assert_eq!(not_json.content, r#""raw output""#);
    }

    #[test]
    fn system_and_empty_messages_are_dropped_and_roles_normalise() {
        assert_eq!(render(&record("system", "you are a helpful agent", "msg_s")), None);
        assert_eq!(render(&record("user", "   ", "msg_e")), None);
        assert_eq!(render(&record("user", "", "msg_z")), None);

        assert_eq!(render(&record("assistant", "hi", "msg_a")).unwrap().role, "assistant");
        assert_eq!(render(&record("user", "hi", "msg_u")).unwrap().role, "user");
        assert_eq!(render(&record("developer", "hi", "msg_d")).unwrap().role, "user");
    }

    #[test]
    fn content_is_json_encoded_and_the_wire_form_is_what_gets_counted() {
        let rendered = render(&record("user", "42", "msg_1")).unwrap();
        assert_eq!(rendered.content, r#""42""#, "text that looks like JSON stays text");
        assert_eq!(
            wire_form(&rendered),
            r#"{"role":"user","content":[{"type":"text","text":"42"}]}"#
        );
    }

    // ── selection ─────────────────────────────────────────────────────────────

    fn seed(text: &str, id: &str) -> SeedMessage {
        render(&record("user", text, id)).unwrap()
    }

    fn ids(messages: &[SeedMessage]) -> Vec<String> {
        messages
            .iter()
            .map(|m| m.source_id.clone().unwrap())
            .collect()
    }

    #[test]
    fn selection_stays_within_the_effective_budget() {
        // Ten identically-sized messages against a budget of 20, whose effective ceiling is
        // the 19 the headroom leaves: the selection fills up to it and stops one short of
        // crossing it.
        let candidates: Vec<SeedMessage> = (0..10)
            .map(|i| seed("alpha beta", &format!("msg_{i}")))
            .collect();
        let per_message = word_count(&wire_form(&candidates[0]));

        let mut spent = 0;
        let chosen = select(candidates, None, 20, |text| {
            let n = word_count(text);
            spent += n;
            n
        });

        assert_eq!(effective_budget(20), 19);
        let committed: u64 = chosen.len() as u64 * per_message;
        assert!(committed <= effective_budget(20), "{committed} over budget");
        assert!(committed + per_message > effective_budget(20), "under-filled");
        assert!(spent > 0, "the injected counter was actually consulted");
    }

    #[test]
    fn without_task_text_selection_prefers_the_newest_messages() {
        let candidates: Vec<SeedMessage> = (0..6)
            .map(|i| seed("alpha beta", &format!("msg_{i}")))
            .collect();

        // Room for exactly three.
        let per_message = word_count(&wire_form(&candidates[0]));
        let chosen = select(candidates, None, per_message * 3 * 100 / 95, word_count);

        assert_eq!(
            ids(&chosen),
            vec!["msg_3", "msg_4", "msg_5"],
            "recency picks the tail, and the result is still oldest-first"
        );
    }

    #[test]
    fn with_task_text_selection_prefers_term_overlap_over_recency() {
        let candidates = vec![
            seed("the parser rejects a trailing comma", "msg_old_relevant"),
            seed("lunch plans for thursday", "msg_mid"),
            seed("the weather is fine today", "msg_newest"),
        ];
        let per_message = word_count(&wire_form(&candidates[0]));

        let chosen = select(
            candidates,
            Some("why does the parser reject a trailing comma?"),
            per_message * 100 / 95,
            word_count,
        );

        assert_eq!(
            ids(&chosen),
            vec!["msg_old_relevant"],
            "the oldest message wins on overlap despite two newer candidates"
        );
    }

    #[test]
    fn a_task_that_tokenises_to_nothing_scores_nothing_and_falls_back_to_recency() {
        let candidates = vec![seed("alpha", "msg_0"), seed("beta", "msg_1")];
        let per_message = word_count(&wire_form(&candidates[0]));

        let chosen = select(candidates, Some("!!! ??? ---"), per_message * 100 / 95, word_count);

        assert_eq!(ids(&chosen), vec!["msg_1"], "newest, not everything");
    }

    #[test]
    fn the_message_cap_bounds_a_seed_a_generous_budget_would_allow() {
        let candidates: Vec<SeedMessage> = (0..MAX_SEEDED_MESSAGES + 20)
            .map(|i| seed("x", &format!("msg_{i}")))
            .collect();

        let chosen = select(candidates, None, u64::MAX / 2, word_count);

        assert_eq!(chosen.len(), MAX_SEEDED_MESSAGES);
    }

    #[test]
    fn terms_are_lowercase_alphanumeric_runs_on_both_sides() {
        assert_eq!(terms("Parser: rejects_trailing-comma!"),
            vec!["parser", "rejects", "trailing", "comma"]);
        assert_eq!(terms("v0.3.0"), vec!["v0", "3", "0"]);
        assert_eq!(terms("!!! ---"), Vec::<String>::new());
    }

    // ── the whole flow ────────────────────────────────────────────────────────

    #[test]
    fn a_seeded_plan_is_chronological_with_source_ids_set() {
        let fake = FakeRecord::new(vec![
            Ok(page(
                vec![
                    record("assistant", "and then I fixed it", "msg_d"),
                    record("user", "the build is broken", "msg_c"),
                ],
                Some("cur1"),
            )),
            Ok(page(
                vec![
                    record("system", "you are a helpful agent", "msg_b"),
                    record("user", "hello", "msg_a"),
                ],
                None,
            )),
        ]);

        let seeds = plan_seed(10_000, 0, |c| fake.read(c), || None, word_count)
            .expect("seeding succeeded");

        assert_eq!(
            ids(&seeds),
            vec!["msg_a", "msg_c", "msg_d"],
            "oldest first, with the system message dropped"
        );
        assert_eq!(seeds[2].role, "assistant");
    }

    #[test]
    fn an_earlier_seeds_copies_are_dropped_so_the_seed_does_not_double_each_run() {
        // What the record holds after one seeded run: the first task's two turns, the
        // second task, then the copies the seed committed, then the second reply.
        let fake = FakeRecord::new(vec![Ok(page(
            vec![
                record("assistant", "reply two", "msg_f"),
                reseeded("assistant", "reply one", "msg_e", "msg_b"),
                reseeded("user", "task one", "msg_d", "msg_a"),
                record("user", "task two", "msg_c"),
                record("assistant", "reply one", "msg_b"),
                record("user", "task one", "msg_a"),
            ],
            None,
        ))]);

        let seeds = plan_seed(10_000, 0, |c| fake.read(c), || None, word_count).unwrap();

        assert_eq!(
            ids(&seeds),
            vec!["msg_a", "msg_b", "msg_c", "msg_f"],
            "each turn is seeded once, from the line it originated on"
        );
    }

    #[test]
    fn a_source_id_pointing_outside_the_walk_is_content_in_its_own_right() {
        let kept = vec![
            reseeded("user", "a corpus note", "msg_a", "rec_00000000"),
            record("assistant", "an ordinary turn", "msg_b"),
        ];

        assert_eq!(
            drop_reseeded(kept.clone()),
            kept,
            "only a copy of a message still in the walk is dropped"
        );
    }

    #[test]
    fn an_empty_record_plans_no_seed_without_erroring() {
        let fake = FakeRecord::new(vec![Ok(page(Vec::new(), None))]);
        let seeds = plan_seed(10_000, 0, |c| fake.read(c), || None, word_count).unwrap();
        assert!(seeds.is_empty());
    }
}
