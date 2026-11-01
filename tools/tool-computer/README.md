# tool-computer

A native tool that gives a capsule a desktop of its own. On the first action it starts a
1440×900 virtual X display (Xvfb) inside the capsule, plus a small window manager when one is
granted and the programs listed in `config: { launch }`. Through one tool call at a time the model
takes screenshots, clicks, drags, scrolls, types, presses keys, waits and zooms. Every action
returns the cursor position and a PNG screenshot as an attachment, so a model that reads images
sees the screen.

Coordinates are display pixels. The manifest declares
`provider_tool: {kind: computer, display_width: 1440, display_height: 900}`, so a driver can offer
the tool as its provider's own computer tool and rescale coordinates to this size.

## Requirements

| Requirement | Why |
|---|---|
| A `mur` built from murmur `a9ea4726` (#381) or later | That `mur` reads `provider_tool:`, refuses the capsule below `sealed` with `E-CAP-003`, and checks the block at `mur build`. An older `mur` ignores the key and would run this tool below `sealed` |
| Linux | The display is Xvfb. On macOS or Windows every call returns `tool-computer runs only on Linux, in a sealed capsule; on macOS or Windows, run the capsule on a Linux VM` |
| `capabilities.containment: sealed` | Under `sealed` the tool runs in the session's own network namespace, so the X server on `127.0.0.1` is reachable by the tool, its programs and the capsule's shell commands and by nothing outside. `/tmp` is private and the host's X server is unreachable. A host that refuses unprivileged user namespaces (many container hosts) cannot give `sealed`, and the capsule is refused with `E-CAP-003` or `E-CAP-005` |
| `Xvfb`, `xkbcomp` and `sh` in `capabilities.shell.allow` | Xvfb runs `xkbcomp` through `/bin/sh -c` to compile its keymap. Without `sh`, Xvfb exits with `Failed to activate virtual core keyboard`, and the tool's start-failure line says to add `sh` |
| Every `config.launch` program in `capabilities.shell.allow` | A native tool's child programs are confined to the capsule's grants |
| A model with `vision: true` on the driver | A `transport: http` model the driver marks `vision: false` is refused with `E-RUN-057` |

### Host packages

`mur deploy run` uploads no host programs, so a VM needs these installed before the capsule runs:

| Package (Debian/Ubuntu) | Gives |
|---|---|
| `xvfb` | `Xvfb` |
| `x11-xkb-utils` | `xkbcomp` |
| `openbox` (optional) | A window manager. The tool starts the first of `openbox`, `fluxbox`, `icewm`, `twm` and `matchbox-window-manager` that is granted and installed; with none, windows have no title bars and cannot be moved |
| The apps to launch | For example `zenity` (forms and dialogs) or `x11-utils` (`xmessage`) |

```bash
sudo apt install xvfb x11-xkb-utils openbox zenity
```

## Actions

`action` is required. Every other field is an optional top-level scalar.

| Field | Type | Actions that take it |
|---|---|---|
| `action` | one of the 16 below | all |
| `x`, `y` | integer, `0 ≤ x < 1440`, `0 ≤ y < 900` | optional as a pair: the five clicks, `left_mouse_down`, `left_mouse_up`, `scroll` (omitted: at the cursor). Required: `mouse_move`, `left_click_drag` (start), `zoom` (top-left) |
| `to_x`, `to_y` | integer, same range | required as a pair: `left_click_drag` (end), `zoom` (bottom-right, exclusive) |
| `text` | string, 1 to 5000 characters | `type` (required) |
| `key` | combos separated by single spaces; a combo is names joined by `+` | `key`, `hold_key` (required) |
| `modifiers` | modifier names joined by `+` | the five clicks, `left_click_drag`, `scroll` |
| `scroll_direction` | `up`, `down`, `left`, `right` | `scroll` (required) |
| `scroll_amount` | integer 1–50, default 3 | `scroll` |
| `duration` | seconds | `wait`: more than 0, at most 60, default 1. `hold_key`: more than 0, at most 30, required |

| Action | Does |
|---|---|
| `screenshot` | Captures the screen |
| `left_click`, `right_click`, `middle_click` | One press and release of button 1, 3 or 2 |
| `double_click`, `triple_click` | Two or three left press/release pairs, 80 ms apart |
| `left_click_drag` | Press at `x,y`, move in 10 steps over about 200 ms, release at `to_x,to_y` |
| `left_mouse_down`, `left_mouse_up` | Press or release button 1; they may come in separate calls |
| `mouse_move` | Moves the pointer |
| `scroll` | One press/release of button 4, 5, 6 or 7 (up, down, left, right) per unit |
| `type` | Types the text, 10 ms between characters. `\n` is Return, `\t` is Tab. A character no key carries is typed through a spare keycode remapped for the call and restored afterwards |
| `key` | Presses each combo and releases it in reverse, combo after combo |
| `hold_key` | Presses every key, waits `duration`, releases them in reverse |
| `wait` | Waits, then captures the screen |
| `zoom` | Captures the region from `x,y` to `to_x,to_y` at its own pixel size, never scaled |

Modifiers are pressed before the action and released after it, in reverse order.

**Key names.** Modifiers, any case: `ctrl`/`control`, `shift`, `alt`, `super`/`meta`/`cmd`/`win`.
Aliases, any case: `enter`, `esc`, `backspace`, `del`, `pageup`/`pgup`, `pagedown`/`pgdn`,
`arrowup`/`up`, `arrowdown`/`down`, `arrowleft`/`left`, `arrowright`/`right`, `space`. X keysym
names, exact case first, then any case: `Return`, `Tab`, `Escape`, `BackSpace`, `Delete`, `Insert`,
`Home`, `End`, `Page_Up`, `Page_Down`, `Left`, `Right`, `Up`, `Down`, `F1`–`F24`, `KP_0`–`KP_9`,
`KP_Enter`, `KP_Add`, `KP_Subtract`, `KP_Multiply`, `KP_Divide`, `KP_Decimal`, `Menu`, `Print`,
`Caps_Lock`, and the left and right `Shift`, `Control`, `Alt` and `Super` keysyms. One printable
character is also a name. `+` alone, or last in a combo (`ctrl++`), is the plus key.

**Refusals.** A call is refused with `status: "error"` and one line naming what is wrong: an unknown
action or field, a field the action does not take (the line lists what it takes), a missing required
field, half a coordinate pair, a wrong type, a coordinate out of range (never clamped), an unknown key
name, a `zoom` with `to_x ≤ x` or `to_y ≤ y`, or a duration out of range. A field whose value is
`null` counts as absent. A refused call starts nothing and creates no `.tool-computer/`.

## Result

One JSON object on stdout:

```json
{"status":"passed","summary":"left_click at 46,60",
 "data":"left_click at 46,60\ncursor: 46,60\nscreenshot: .tool-computer/ses_x/screen-3.png (1440x900)",
 "attachments":[{"media_type":"image/png","path":".tool-computer/ses_x/screen-3.png"}]}
```

`data` is, line by line:

1. The action line: `screenshot`, `left_click at X,Y` or `left_click at the cursor` (plus
   ` with <modifiers>`), `left_click_drag from X,Y to X2,Y2`, `left_mouse_down at …`,
   `mouse_move to X,Y`, `scroll down by 3 at the cursor`, `typed 12 characters`, `key ctrl+a Return`,
   `hold_key shift for 0.2 s`, `waited 1 s`, `zoom of X,Y to X2,Y2 (WxH at 1:1)`.
2. `cursor: X,Y`, read after the action.
3. `screenshot: <work-folder-relative path> (<w>x<h>)`.
4. One line per launched program that exited since the last call:
   `launched <argv0> (launch[<i>]) exited with status <n>; output in <log path>`.

The first result after a display starts opens with the banner:

```text
display: Xvfb 127.0.0.1:0, 1440x900x24; window manager none (grant one of openbox, fluxbox, icewm, twm or matchbox-window-manager); launched zenity; shell commands reach it with DISPLAY=127.0.0.1:0 XAUTHORITY=/…/work/.tool-computer/ses_…/Xauthority
```

After a restart (Xvfb exited since the last call) the banner is preceded by
`the previous display stopped; programs on it are gone`.

After an input action the tool samples the screen every 100 ms from 150 ms on, until two samples
match or 2 s pass, so the screenshot shows the screen once it has settled. `screenshot` and `zoom`
take one sample.

## Config

`config:` on the tool's artifact entry is unset, or an object with only these keys:

| Key | Type | Default | Meaning |
|---|---|---|---|
| `launch` | list of at most 16 non-empty lists of strings | `[]` | Each is the argv of a program started, in order, once the display is up. `argv[0]` is a bare name, never a path, and must be in `capabilities.shell.allow` |
| `confirm` | list of action names | `[]` | Action kinds the tool holds and never runs |

Every config error fails every call before anything starts, with one line, for example
`tool-computer: config launch[0] runs zenity, which capabilities.shell.allow does not grant`.

### Why `confirm` holds rather than asks

A listed action returns, with `status: "error"` and no screenshot:

```text
tool-computer did not run left_click: this capsule's config lists left_click in confirm, so it needs a person's yes, and murmur gives a native tool no way to ask for one; ask the person to do this step, or to remove left_click from confirm
```

murmur asks a person before a tool call only when a **driver** attaches `safety_checks` to the call
it returns; the runtime then waits for the person's reply, up to `lifecycle.input_timeout_secs`.
A native tool's result holds only `status`, `summary`, `data` and `attachments`, so the tool has no
channel to ask, and anything it could treat as a yes (a file, a token passed back, a
`tool-request-input` call) the model could forge. Holding is the safe reading of the list. A
driver that maps this tool onto its provider's computer tool may raise the provider's own safety
checks, which then do pause for a person.

## Example capsule

```yaml
name: desktop-form
version: 0.1.0
artifacts:
  - name: murmur/driver-anthropic
    version: 0.18.0
    runtime: driver
    gateway:
      endpoint: https://api.anthropic.com
      api_key: ${ANTHROPIC_API_KEY}
  - name: murmur/tool-computer
    version: 0.1.0
    runtime: tool
    config:
      launch:
        - [zenity, --forms, --title=Signup, --add-entry=Name, --add-entry=Email]
      confirm: [middle_click]
capabilities:
  containment: sealed
  shell:
    allow: [Xvfb, xkbcomp, sh, openbox, zenity]
  resources:
    memory_bytes: 8589934592        # RLIMIT_AS per process; GTK apps reserve over 2 GiB
    cgroup_memory_bytes: 4294967296 # the default; the whole tree peaked at 203 MB
    cgroup_pids_max: 256            # the default; the whole tree peaked at 31 tasks
inference:
  transport: http
  model: claude-fable-5-1
  driver:
    artifact: murmur/driver-anthropic
lifecycle:
  input_timeout_secs: 300           # how long a driver's safety checks wait for a person
```

### Measured under `sealed`

Ubuntu 24.04.5, kernel 7.0.0-28, Xvfb 21.1.12, `mur` at murmur `ccd18c08` (which carries #378,
#379 and #381):

| Program | VmPeak | Peak RSS (VmHWM) | Session tree |
|---|---|---|---|
| Xvfb alone | 241 MiB | 69 MiB | — |
| Xvfb + `xmessage` | — | — | 8 tasks, 35 MiB `memory.peak` |
| `zenity --forms` | 2.20 GiB | 256 MiB | 31 tasks, 194 MiB `memory.peak` with Xvfb |

With the default `memory_bytes` of 2 GiB, `zenity` dies with SIGSEGV (status 139) while it starts.
8 GiB covers it. Xvfb and `xmessage` run within every default. No `interpreter_runtime` grant was
needed: Xvfb found its keymaps and fonts, and Xvfb's compiled keymap went to the capsule's private
`/tmp`. GTK prints `Fontconfig error: Cannot load default config file` and falls back to built-in
font settings.

## Opening an app from a shell command

The banner gives the display's address and cookie file. A shell command run by the capsule shares
the session's loopback, so with the program granted:

```bash
DISPLAY=127.0.0.1:0 XAUTHORITY=/…/work/.tool-computer/ses_…/Xauthority zenity --info --text hi &
```

The tool connects only to its own display and never reads `DISPLAY` or `XAUTHORITY`. A client
without the cookie is refused.

## State folder

`<work folder>/.tool-computer/<session-id>/`, where `<session-id>` is the name of
`$MURMUR_RUNTIME_DIR`'s parent:

| File | Holds |
|---|---|
| `display.json` (0600) | `display`, `address`, `width`, `height`, `keeper_pid`, `xvfb_pid`, `window_manager` (`{name, pid}` or `null`), `launched` (`[{index, argv, pid, log}]`), `started_at` (Unix seconds) |
| `Xauthority` (0600) | One FamilyWild MIT-MAGIC-COOKIE-1 entry, 16 random bytes, naming the display |
| `keeper.log` | Xvfb's, the window manager's and the keeper's output, cut at 1 MiB |
| `launch-<i>-<argv0>.log` | Each launched program's stdout and stderr, cut at 1 MiB |
| `exits.jsonl`, `exits.seen` | Exits of launched programs (`{"index","argv0","status"}`), and how many were already reported |
| `stopped.json` | Why the last display stopped, or why it did not start |
| `start.lock` | Held while a call starts the display; stale after 30 s |
| `screen-<n>.png` | Screenshots; the newest 20 are kept. The session record keeps its own copies by sha256 |

The first call that passes every check re-executes the binary as a detached keeper
(`tool-computer --keep <state folder>`, in a new session, with stdio away from the runtime's pipes).
The keeper starts `Xvfb -displayfd <fd> -screen 0 1440x900x24 -auth <Xauthority> -nolisten unix
-nolisten local -listen tcp -noreset -dpi 96 +extension XTEST`, then the window manager and the
`launch` programs, writes `display.json`, and checks once a second. When Xvfb exits it stops the
rest and writes `stopped.json`; when the session's runtime folder is gone it stops everything. At
session end murmur also kills every process in the session's cgroup.
