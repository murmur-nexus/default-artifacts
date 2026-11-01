# Does Claude Code show its model an image from a murmur tool?

Two procedures check this. Neither is run by `cargo test`: Cargo ignores this directory, which
has no `main.rs`. No file here is named `murmur.yaml`. The live check writes its fixture tool's
manifest into a scratch directory.

| File | What it is |
|---|---|
| `picture.py` | Draws a PNG of a three-digit number on a red, green, blue or yellow background |
| `image_bridge.py` | A mock MCP server whose one tool, `picture_tool`, answers the way murmur's bridge does: a text item, then an MCP `image` item |
| `record_image_mock.py` | Records real `claude` against `mock.py` and `image_bridge.py`, and reads what `claude` sent its model |
| `image-check.sh` | The live check: a real capsule on a real subscription |

All of them use only the Python standard library and need `python3`.

## The mock recording

This needs no subscription and no murmur. `claude` runs with the driver's bridged argv
against two loopback mocks:

- `mock.py`, a scripted Anthropic endpoint. It is untracked, kept in the main checkout at
  `.nexus/roadmap/fixtures/tools/mock.py`.
- `image_bridge.py`.

`claude` gets a fresh scratch `HOME`, a fresh working directory and a dummy API key. It never
sees the real `HOME`.

    python3 -I drivers/driver-claude-code/tests/live/picture.py --out /tmp/picture.png \
        --number 482 --background blue
    python3 -I drivers/driver-claude-code/tests/live/record_image_mock.py \
        --mock <path to mock.py> --image /tmp/picture.png --out /tmp/image-recording

It writes `12-bridge-image-result.{stdout.jsonl,stderr.txt,exit}` into `--out`, with paths
redacted the way `tests/fixtures/README.md` describes. It keeps its scratch directory, which
holds the mock's request log. It then prints:

- the `tool_result` block of the request `claude` sent after the tool call, with each base64 cut
  to its ends and length;
- a classification: (a) an `image` block with the bridge's exact bytes, (b) an `image` block
  re-encoded, or (c) no image block;
- the shape of the image on the stdout `user` line.

`claude` 2.1.316 gives (a) for the committed 692-byte PNG. For a 12 MB 2000×2000 noise PNG it
gives (b): a 1932×1932 JPEG.

## The live check

This spends one turn of the Claude subscription `claude` is logged in to.

Prerequisites:

- **A `mur` built from murmur `4f65bb98` or later.** That commit's tool bridge sends MCP image
  items. Without it there is no image to show. The script checks the `mur version` commit
  against a murmur checkout and refuses an older `mur` before building anything. To build one:

      mkdir /tmp/murmur-src
      git -C /space/_murmur/murmur fetch
      git -C /space/_murmur/murmur archive origin/main | tar -x -C /tmp/murmur-src
      (cd /tmp/murmur-src && CARGO_PROFILE_DEV_OPT_LEVEL=1 CARGO_PROFILE_DEV_DEBUG=0 \
          cargo build --locked -p murmur-cli)

- **`claude` on `PATH`, logged in to a subscription** (`claude` and then `/login` once).
- **`python3`.**

Run it from the repo root:

    MUR_BIN=/tmp/murmur-src/target/debug/mur MURMUR_DIR=/space/_murmur/murmur MODEL=haiku \
        drivers/driver-claude-code/tests/live/image-check.sh

| Env | Default | What it is |
|---|---|---|
| `MUR_BIN` | `mur` on `PATH` | the `mur` to run; its directory goes first on `PATH`, because `package-artifact.sh` runs `mur build` from there |
| `MURMUR_DIR` | `/space/_murmur/murmur` | a murmur git checkout, to check the `mur` commit against |
| `MODEL` | `haiku` | `inference.model` |

The script:

1. makes `/tmp/cc-image-check.XXXXXX`, holding `capsule/` and, outside it, `expected.txt`;
2. draws `capsule/picture.png` with a random number and colour;
3. writes the capsule's `murmur.yaml`;
4. packs a native tool `picture-tool` with `mur build` and installs it with `mur install`. The
   tool answers `PICTURE-TOOL-RAN` and attaches `picture.png`;
5. installs the driver with `scripts/local-install.sh driver-claude-code <capsule>`;
6. writes `task.md`, which asks for `NUMBER=<n> BACKGROUND=<colour>`, or `NO-PICTURE` rather
   than a guess;
7. runs `mur run --task task.md --workdir <capsule>` from the capsule;
8. reads the newest session that `mur trace list --workdir <capsule> --json` names, and prints:
   - the expected values;
   - the model's answer, from `out/result.txt`;
   - the session's auth, from the trace's `harness_session`;
   - the `tool_call` record's `output` and `attachments`;
   - any harness warning or failure;
   - the verdict.

| Verdict | Meaning | Exit |
|---|---|---|
| `PASS` | the number and the colour match, case-insensitive: the model saw the picture | 0 |
| `NOT SHOWN` | the model answered `NO-PICTURE`: `claude` did not show it the image | 1 |
| `WRONG` | anything else, including no session at all. The end of `mur run`'s output is printed | 1 |

The expected values are written only to `expected.txt`. The capsule, its work folder, the task
and the tool never hold them.
