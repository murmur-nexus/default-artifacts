# tool-nexus-trigger

A capsule's way to ask Nexus to wait. The tool adds, lists, shows and cancels the triggers of the
work folder through its trigger inbox, which a Nexus window open on that folder answers. Every
trigger it adds waits for a person to type `/trigger approve <id>` in that window, and does nothing
before then.

The capsule needs no shell grant: no `nexus`, `env` or `node`, no OpenSSL config and no staged
Node tree. The entry is granted the inbox folder and nothing else.

WASM component (`wasm32-wasip2`), exporting `murmur:tool/run` and importing no `murmur:*`
interface.

| Needs | Version |
|---|---|
| `mur` | A `mur` serving WIT level 3 or later, with `capabilities.filesystem.scope` on tool entries |
| `nexus` | A build with the trigger inbox protocol v1: murmur-nexus `main` from `1a8aba3` (#239). No nexus release has it yet |

## The manifest entry

```yaml
artifacts:
  - name: murmur/tool-nexus-trigger      # local/tool-nexus-trigger after local-install.sh
    version: "0.1.0"
    runtime: tool
    capabilities:
      filesystem:
        scope: .nexus/triggers/inbox     # the inbox, never the work folder
```

The scope mounts `<work folder>/.nexus/triggers/inbox` as the tool's `.`, and the runtime creates
the folder when it is missing. Only the operator's entry can grant it; the bundled `murmur.yaml`
declares no `capabilities:` block.

| Entry | What the tool does |
|---|---|
| `scope: .nexus/triggers/inbox` | Works |
| No scope | Refuses every call with `WHOLE_WORKDIR` and writes nothing: without a scope the entry reaches the whole work folder (the runtime then also hands it `murmur-runtime`, which is how the tool tells) |
| Any other scope, such as `cache` | Writes its request into that folder, where no window looks, and answers `NO_ANSWER` after 5 s, leaving the folder as it found it |

## The four ops

Input is one JSON object. `op` is required.

| Op | Fields | What Nexus does |
|---|---|---|
| `add` | `until`, `every`, `for`, `label`, `at`, `cron`, `wake`, `scope`, `reaches`, `onEvent`, `card`, `transitEvent`, `eventCard`, all text, all optional | Writes the trigger awaiting approval, as `nexus trigger add` does, and answers with its id |
| `list` | none | One line per trigger in the folder |
| `show` | `id` (text, required) | One trigger's record and history |
| `cancel` | `id` (text, required), `yes` (true or false) | Ends a live trigger when `yes` is true; without it cancels nothing and says so |

Each `add` field is the flag of `nexus trigger add` of the same name: `until` is `--until` (the
check, a shell command Nexus runs on the host as the person), `onEvent` is `--on-event`,
`transitEvent` is `--transit-event` and `eventCard` is `--event-card`. Nexus checks every value:
durations, ranges, the cron expression, the command's length, which fields go together, and the
live and awaiting limits. `--project` and `-w` are not part of the inbox protocol.

A field sent as `null` counts as not sent, and the tool adds no default. `approve`, `refuse` and
`renew` are a person's, in the window; the tool refuses them with `PERSON_ONLY`.

## The result

| Nexus answered | Status |
|---|---|
| exit 0 | `passed` |
| any other exit | `failed` |
| The tool's own line (a refusal, no answer, an unreadable answer) | `failed` for an input refusal, `error` otherwise |

`data` is Nexus's answer as JSON. The model reads `data`, not `summary`, so it carries the summary
too:

```json
{"ok":true,"op":"add","exit":0,"summary":"Requested: it waits for a person's approval in a Nexus window and does not run until then. trigger t2 awaits approval: a person types /trigger approve t2 in a nexus window open on /home/me/project","out":["trigger t2 awaits approval: a person types /trigger approve t2 in a nexus window open on /home/me/project"],"err":[]}
```

`out` and `err` are every line `nexus trigger` prints, unchanged. `summary` is the first `out`
line on exit 0 and the first `err` line otherwise, falling back to the other list. A passed
`add`'s summary always starts with `AWAITS_APPROVAL`. A line the tool writes itself comes back as
`{"ok":false,"op":<op or null>,"error":<line>}`.

## Fixed messages

Every line the tool writes itself starts `tool-nexus-trigger: `.

| Name | Text |
|---|---|
| `AWAITS_APPROVAL` | `Requested: it waits for a person's approval in a Nexus window and does not run until then.` |
| `WHOLE_WORKDIR` | `tool-nexus-trigger: this entry reaches the whole work folder; give it capabilities.filesystem.scope: .nexus/triggers/inbox so it reaches the trigger inbox and nothing else. Nothing was written.` |
| `NO_ANSWER` | `tool-nexus-trigger: no Nexus window answered within 5 s, so the request was removed and no trigger was created. Only a Nexus window open on this work folder answers the trigger inbox (the transit daemon does not read it), and this entry's capabilities.filesystem.scope must be .nexus/triggers/inbox.` |
| `TAKEN_NO_ANSWER` | `tool-nexus-trigger: a Nexus window took the request but sent no answer within 5 s, so a trigger may have been created; call list to see.` |
| not removed | `tool-nexus-trigger: no Nexus window answered within 5 s, and the request could not be removed (<error>), so a window may still take it; call list to see.` |
| `CANNOT_WRITE` | `tool-nexus-trigger: cannot write the trigger inbox: <error>. Nothing was written.` |
| `MALFORMED_ANSWER` | `tool-nexus-trigger: the answer from Nexus cannot be read (<reason>); the request may have taken effect, so call list to see.` |
| `LABEL_TOO_LONG` | `tool-nexus-trigger: label is <n> characters, over Nexus's limit of 80. Nothing was written.` |
| `REQUEST_TOO_LARGE` | `tool-nexus-trigger: the request is <n> bytes, over the trigger inbox's limit of 16384 bytes; shorten until, wake or label. Nothing was written.` |
| `PERSON_ONLY` | `tool-nexus-trigger: <op> happens only in a Nexus window, by a person: /trigger <op> <id>. Nothing was written.` |
| not an object | `tool-nexus-trigger: the input is not a JSON object; send {"op": "list"}, or op add, show or cancel with that op's fields. Nothing was written.` |
| no op | `tool-nexus-trigger: the input has no op; op is one of add, list, show, cancel. Nothing was written.` |
| unknown op | `tool-nexus-trigger: op "<name>" is not one of add, list, show, cancel. Nothing was written.` |
| unknown field | `tool-nexus-trigger: <op> does not take "<field>"; <op> takes <fields>. Nothing was written.` |
| not text | `tool-nexus-trigger: <field> must be text; <op> takes <fields>. Nothing was written.` |
| `yes` not a boolean | `tool-nexus-trigger: yes must be true or false, for cancel. Nothing was written.` |
| missing id | `tool-nexus-trigger: <op> needs id, the trigger's id such as t3 (list shows them); <op> takes <fields>. Nothing was written.` |
| no lines | `tool-nexus-trigger: Nexus answered exit <n> with no lines.` |

`MALFORMED_ANSWER`'s reasons are `not a JSON object`, `its keys are not exactly v, exit, out and
err`, `v is not 1`, `exit is not an integer`, `out is not a list of lines`, `err is not a list of
lines`, `a link, not a file`, `not a regular file`, `larger than 256 KiB` and `cannot be read
(<error>)`.

## One call

The tool speaks the writer's side of murmur-nexus's trigger inbox protocol v1
(`docs/content/reference/trigger-inbox.md` in murmur-nexus) in its structured form,
`{"v":1,"op":…,…}`:

1. Refuse a whole-work-folder grant. Nothing is written and no folder is made.
2. Check the input against the protocol's shape: which op takes which field, and each field's
   type. The tool's own checks end with two numbers Nexus publishes: a label of at most 80
   characters, and a request of at most 16384 bytes.
3. Create `<rid>.tmp` exclusively, write the request, flush it, and rename it to `<rid>.json`.
   `<rid>` is 16 lowercase hex digits from 8 bytes of `wasi:random`.
4. Every 50 ms for at most 5 s on the monotonic clock, look for `<rid>.answer.json`. Read it only
   when it is a regular file of at most 256 KiB, then remove it. A link there is removed without
   being followed, and a folder there is left alone.
5. With no answer in time, remove `<rid>.json`. When it is already gone, a window renamed it to
   `<rid>.taken` and may have acted, so the tool looks for the answer once more and otherwise
   says to call `list`.

The tool touches only `<rid>.tmp`, `<rid>.json` and `<rid>.answer.json`, never lists the inbox,
never creates a folder, and makes one request per call with no retry. A call takes at most 5 s
plus 50 ms plus its file I/O.

## Limits

The tool checks only the label's length (80 characters) and the request's size (16384 bytes),
before it writes anything. Approval, ranges, defaults, the 16 live and 4 awaiting triggers per
folder, and the cancel rule all stay in Nexus, which answers them. The tool never opens Nexus's
trigger store or trigger files, and starts no program.

## Built against

murmur-nexus `origin/main` at `1a8aba3fcb220202776b363c117a185292ee568f` ("The trigger inbox has no
written contract and an open race, so a second writer cannot rely on it", #239), whose
`docs/content/reference/trigger-inbox.md` and `tests/triggers.inboxProtocol.test.ts` pin protocol
v1.
