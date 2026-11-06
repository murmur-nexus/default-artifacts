# tool-computer

Gives a capsule a desktop of its own. On the first action it starts a 1440×900 virtual X display
(Xvfb), plus a small window manager when one is granted and the programs listed in
`config: { launch }`. Through one tool call at a time the model takes screenshots, clicks, drags,
scrolls, types, presses keys, waits and zooms. Every action returns the cursor position and a PNG
screenshot as an attachment, so a model that reads images sees the screen.

```bash
mur install -g murmur/tool-computer@0.2.0
```

The tool is a WASM component that imports `murmur:process`. Xvfb, the window manager and every
launched program are session processes: murmur starts them under the capsule's grants and stops
them all when the session ends. The tool speaks the X protocol to its own Xvfb over a
`murmur:process` connection to the display's loopback port.

Coordinates are display pixels. The manifest declares
`provider_tool: {kind: computer, display_width: 1440, display_height: 900}`, so a driver can offer
the tool as its provider's own computer tool and rescale coordinates to this size.

## Requirements

| Requirement | Why |
|---|---|
| A `mur` whose `mur version` reports WIT level 4 or higher: murmur `f9d4a1f8` (#390) or later, which no murmur release holds yet | The tool imports `murmur:process`. An older `mur` installs the zip, then refuses the capsule at launch with `error[E-WIT-LEVEL-001]: artifact 'murmur/tool-computer@0.2.0' does not fit WIT level 3, the level this mur serves: it imports murmur:process/process@0.1.0, and level 3 has no version of murmur:process` |
| A Linux host with Xvfb | The display is Xvfb, started on the host the capsule runs on |
| `capabilities.containment: sealed` | murmur runs a tool declaring `provider_tool:` only under `sealed`, and refuses anything weaker with `E-CAP-003`. Under `sealed` the X server listens inside the session's own network namespace, so it is reachable by the tool, its programs and the capsule's shell commands, and by nothing outside. A host that refuses unprivileged user namespaces (many container hosts) cannot give `sealed` |
| A model with `vision: true` on the driver | A `transport: http` model the driver marks `vision: false` is refused with `E-RUN-057` |
| An entry without `capabilities.filesystem.scope` | The tool reads the capsule's grants from the runtime directory murmur preopens, which it does not preopen for an entry that declares a scope. Such an entry answers `tool-computer cannot see which programs capabilities.shell.allow grants, because its entry declares capabilities.filesystem.scope; tool-computer runs only from an entry without one` |

### What the capsule grants

`murmur:process` refuses to start any program `capabilities.shell.allow` does not list, and puts
each one it starts to the `capabilities.filesystem.read_only` check and the `on-shell` hooks first.

| `capabilities.shell.allow` entry | For |
|---|---|
| `Xvfb` | The display. Without it every call answers `tool-computer needs a display server: add Xvfb and xkbcomp to capabilities.shell.allow` |
| `xkbcomp` | Xvfb compiles its keymap with it |
| `sh` | Xvfb runs `xkbcomp` through `/bin/sh -c`. Without `sh`, Xvfb exits with `Failed to activate virtual core keyboard`, and the tool's start-failure line ends `Xvfb compiles its keymap by running xkbcomp through sh, so add sh to capabilities.shell.allow` |
| One window manager (optional) | The tool starts the first of `openbox`, `fluxbox`, `icewm`, `twm` and `matchbox-window-manager` that is granted and starts. With none, windows have no title bars and cannot be moved |
| Every `config.launch` program | Each is started as its own session process |

### Host packages

`mur deploy run` uploads no host programs, so a VM needs these installed before the capsule runs:

| Package (Debian/Ubuntu) | Gives |
|---|---|
| `xvfb` | `Xvfb`. Without it the first call answers `tool-computer: capabilities.shell.allow grants Xvfb, but no Xvfb is on PATH; install the xvfb package on this host first (mur deploy run does not upload host programs)` |
| `x11-xkb-utils` | `xkbcomp`. Without it Xvfb exits, and the start-failure line ends `install the x11-xkb-utils package on this host first` |
| `openbox` (optional) | A window manager |
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

```json
{"status":"passed","summary":"left_click at 46,60",
 "data":"left_click at 46,60\ncursor: 46,60\nscreenshot: .tool-computer/9bf048013a992480/screen-3.png (1440x900)",
 "attachments":[{"media_type":"image/png","path":".tool-computer/9bf048013a992480/screen-3.png"}]}
```

`data` is, line by line:

1. The action line: `screenshot`, `left_click at X,Y` or `left_click at the cursor` (plus
   ` with <modifiers>`), `left_click_drag from X,Y to X2,Y2`, `left_mouse_down at …`,
   `mouse_move to X,Y`, `scroll down by 3 at the cursor`, `typed 12 characters`, `key ctrl+a Return`,
   `hold_key shift for 0.2 s`, `waited 1 s`, `zoom of X,Y to X2,Y2 (WxH at 1:1)`.
2. `cursor: X,Y`, read after the action.
3. `screenshot: <work-folder-relative path> (<w>x<h>)`.
4. On the call that started the display, one line per `launch` program murmur refused to start:
   `launched <argv0> (launch[<i>]) was not started: murmur refused it: <rule>: <reason>`.
5. One line per program that exited since the last call, each reported once:
   `launched <argv0> (launch[<i>]) exited with status <n>; output in <log path>`, or
   `window manager <name> exited with status <n>; output in .tool-computer/<id>/display.log`. A
   `launch` program missing from `PATH` is reported as exited with status 127, and its log holds
   `tool-computer: cannot start <argv0>: No such file or directory (os error 2)`.

The first result after a display starts opens with the banner:

```text
display: Xvfb 127.0.0.1:0, 1440x900x24; window manager none (grant one of openbox, fluxbox, icewm, twm or matchbox-window-manager); launched zenity; shell commands reach it with DISPLAY=127.0.0.1:0 XAUTHORITY=.tool-computer/9bf048013a992480/Xauthority
```

After a restart (Xvfb exited since the last call) the banner is preceded by
`the previous display stopped; programs on it are gone`.

A display that does not start answers
`tool-computer: the display did not start: <reason>; the last lines of .tool-computer/<id>/display.log: <the last three lines>`,
where the reason is `Xvfb exited with status <n>` or `Xvfb gave no display number within 15 s`.

After an input action the tool samples the screen every 100 ms from 150 ms on, until two samples
match or 2 s pass, so the screenshot shows the screen once it has settled. `screenshot` and `zoom`
take one sample.

## Config

`config:` on the tool's artifact entry is unset, or an object with only these keys:

| Key | Type | Default | Meaning |
|---|---|---|---|
| `launch` | list of at most 6 non-empty lists of strings | `[]` | Each is the argv of a program started, in order, once the display is up. `argv[0]` is a bare name, never a path, and must be in `capabilities.shell.allow` |
| `confirm` | list of action names | `[]` | Action kinds the tool holds and never runs |

`launch` holds at most 6 because murmur runs at most 8 session processes per session, counting
every tool's, and Xvfb and the window manager take two. A program the cap refuses is reported with
its `was not started` line, and the display still starts.

Every config error fails every call before anything starts, with one line, for example
`tool-computer: config launch[0] runs zenity, which capabilities.shell.allow does not grant` or
`tool-computer: config launch holds 7 programs; at most 6 are allowed`.

### Why `confirm` holds rather than asks

A listed action returns, with `status: "error"` and no screenshot:

```text
tool-computer did not run left_click: this capsule's config lists left_click in confirm, so it needs a person's yes, and a tool cannot ask a person for one; ask the person to do this step, or to remove left_click from confirm
```

murmur asks a person before a tool call only when a **driver** attaches `safety_checks` to the call
it returns; the runtime then waits for the person's reply, up to `lifecycle.input_timeout_secs`.
A tool's result holds only a status, a summary, its text and attachments, so the tool has no
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
    version: 0.20.0
    runtime: driver
    gateway:
      endpoint: https://api.anthropic.com
      api_key: ${ANTHROPIC_API_KEY}
  - name: murmur/tool-computer
    version: 0.2.0
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
inference:
  transport: http
  model: claude-fable-5-1
  driver:
    artifact: murmur/driver-anthropic
lifecycle:
  input_timeout_secs: 300           # how long a driver's safety checks wait for a person
```

With the default `memory_bytes` of 2 GiB, `zenity` dies with SIGSEGV (status 139) while it starts;
8 GiB covers it. Xvfb and `xmessage` run within every default.

## Opening an app from a shell command

The banner gives the display's address and cookie file. A shell command run by the capsule shares
the session's loopback and runs in the work folder, where the relative `XAUTHORITY` path resolves,
so with the program granted:

```bash
DISPLAY=127.0.0.1:0 XAUTHORITY=.tool-computer/9bf048013a992480/Xauthority zenity --info --text hi &
```

The tool connects only to its own display and never reads `DISPLAY` or `XAUTHORITY`. A client
without the cookie is refused.

## State folder

`<work folder>/.tool-computer/<id>/`, one per display started, where `<id>` is 16 random lowercase
hex digits:

| File | Holds |
|---|---|
| `state.json` | `handle` (the `display` session process), `display`, `port` (6000 + display), `address`, `width`, `height`, `window_manager` (`{name, handle}` or `null`), `launched` (`[{index, argv, handle, log, refused}]`, `handle` `null` for a program not started), `reported_exits` (the handles whose exit a result reported), `started_at` (Unix seconds) |
| `Xauthority` | One FamilyWild MIT-MAGIC-COOKIE-1 entry, 16 random bytes, naming the display |
| `display.log` | Xvfb's and the window manager's output, cut at 1 MiB |
| `launch-<i>-<argv0>.log` | Each launched program's stdout and stderr, cut at 1 MiB |
| `screen-<n>.png` | Screenshots; the newest 20 are kept. The session record keeps its own copies by sha256 |

A WASM tool cannot set a file's mode, so `Xauthority` gets the host's default mode rather than
0600. Under `sealed` the X server is reachable only inside the session's network namespace, so the
cookie guards it only against programs in the same session.

## How a call runs

The first call that passes every check starts the display:

1. It writes `Xauthority` with a new cookie, then starts
   `Xvfb -displayfd 1 -screen 0 1440x900x24 -auth .tool-computer/<id>/Xauthority -nolisten unix -nolisten local -listen tcp -noreset -dpi 96 +extension XTEST`
   as the session process `display`, and reads the display number from its stdout.
2. It connects to port 6000 + the display number through `murmur:process`, which allows the
   connection because Xvfb listens there, and completes the X setup with the cookie.
3. It starts the window manager as `wm` and each `launch` program as `launch-<i>`, with
   `DISPLAY=127.0.0.1:<n>` and `XAUTHORITY=.tool-computer/<id>/Xauthority`, and waits until the
   number of windows on screen has held for 500 ms, at most 5 s.
4. It writes `state.json`, then does the action.

Every later call finds `display` by name, reconnects, does one action, then reads what each
program printed into its log and reports the ones that exited. When Xvfb has exited, the next call
stops the old window manager and programs and starts a new display in a new folder. murmur kills
every session process when the session ends.

## What changed in 0.2.0

- The tool is a WASM component, one zip for every platform. It needs a `mur` reporting WIT level 4
  or higher.
- Xvfb, the window manager and the `launch` programs are session processes. murmur admits each
  through `capabilities.shell.allow`, the `capabilities.filesystem.read_only` check and the
  `on-shell` hooks, and kills them when the session ends.
- `launch` holds at most 6 programs (it was 16).
- A `launch` program missing from `PATH` no longer refuses the call before the display starts; it
  is reported as exited with status 127.
- The check for `xkbcomp` on `PATH` is gone; a keymap that fails to compile gives a hint on the
  start-failure line instead.
- The line for an OS other than Linux is gone.
- The held line ends `and a tool cannot ask a person for one; …`.
- An entry that declares `capabilities.filesystem.scope` is refused.
- The state folder is `.tool-computer/<random id>/`, one per display, instead of
  `.tool-computer/<session-id>/`. `display.json` is replaced by `state.json` and `keeper.log` by
  `display.log`; `stopped.json`, `exits.jsonl`, `exits.seen` and `start.lock` are gone. The banner's
  `XAUTHORITY` is relative to the work folder.
