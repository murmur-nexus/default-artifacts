# tool-browser

Drives a headless Chrome or Chromium from a capsule, one action per tool call: open a page, read it
as text, click, type, press keys, choose options, wait, go back, read the console and failed
requests, and save screenshots.

The browser is the Chrome or Chromium already on the host. It starts on the first browser call,
later calls reuse it, and it stops after 15 idle minutes or at session end. Nothing runs at capsule
launch.

```bash
mur install -g murmur/tool-browser@0.1.0
```

## Actions

Every call takes `action` and the fields that action needs. Fields an action does not use are
ignored, and a `null` counts as absent.

| `action` | Needs | Optional | Returns |
|---|---|---|---|
| `open` | `url` | | The full snapshot. `page still loading after 30 s` is added below it when the load event did not fire in time |
| `snapshot` | | `ref` | The full snapshot, or the subtree under `ref` |
| `click` | `ref` | | A change summary |
| `type` | `ref`, `text` | `submit` | A change summary. `submit: true` presses Enter after the text |
| `press` | `key` | | A change summary |
| `select` | `ref`, `value` | | A change summary. `value` is the option's value, else its visible label |
| `wait` | one of `text`, `ref`, `ms` | | A change summary |
| `back` | | | A change summary, or `nothing to go back to` |
| `console` | | | The console entries since the last `console` call, or `no new console messages` |
| `network` | | | Failed requests and responses with status 400 or above since the last `network` call, or `no failed requests` |
| `screenshot` | | `full_page`, `ref` | `saved <path> (<w>x<h>, <n> KB)` |
| `close` | | | `browser closed`. The next call starts a new browser |

| Field | Type | Meaning |
|---|---|---|
| `url` | string | An `http`, `https` or `file` URL, `about:blank`, or a path to a file inside the work folder. `javascript:`, `data:` and `chrome:` are refused, and so is a path that leaves the work folder |
| `ref` | string | A ref from the last snapshot, such as `e3` |
| `text` | string | `type`: the text to insert. `wait`: the text to wait for in the page |
| `key` | string | `Enter`, `Tab`, `Escape`, `Backspace`, `Delete`, `Space`, `ArrowUp`, `ArrowDown`, `ArrowLeft`, `ArrowRight`, `Home`, `End`, `PageUp`, `PageDown`, or one character, optionally after `Control+`, `Shift+`, `Alt+` or `Meta+` |
| `value` | string | `select`: the option to choose |
| `submit` | boolean | `type`: press Enter after the text |
| `full_page` | boolean | `screenshot`: the whole page instead of the 1280×800 viewport |
| `ms` | integer | `wait`: the timeout for `text` or `ref`, or with neither, how long to sleep. Default 10 000, at most 60 000 |

A refused call is `status: "error"` with one line naming the field and the action, such as
`click needs ref (a ref such as e3 from a snapshot)`.

### Snapshots

```text
page: Fixture Page — http://127.0.0.1:8000/
- heading "waiting" [level=1] [ref=e1]
- button "Go" [ref=e2]
- paragraph [ref=e3]
  - text "Name" [ref=e4]
  - textbox "Name" [value="Ada"] [ref=e5]
```

The first line is the page's title and URL. Then each node of the page's accessibility tree is one
line, indented two spaces per level: `- <role> "<name>" [<states>] [ref=eN]`.

- Ignored, `none` and unnamed `generic` nodes are folded into their parent, and a text node that
  repeats its parent's name is dropped.
- A ref stays with its element across snapshots of one document, and a new element takes the next
  number. A new document starts again at `e1`.
- A ref the current document did not give out is refused with `unknown ref eN; take a snapshot first`.
- A snapshot is cut at 20 000 characters, ending in
  `… cut at 20000 characters; pass ref to snapshot one part`.

### Change summaries

`click`, `type`, `press`, `select`, `wait` and `back` wait for any navigation they started, then
compare a new snapshot with the previous one:

```text
changed: +1 -1 lines
- - heading "waiting" [level=1] [ref=e1]
+ - heading "clicked" [level=1] [ref=e1]
console: 1 new error(s); call console
```

| Line | When |
|---|---|
| `url: …`, `title: …` | The URL or title changed |
| `changed: +A -R lines`, then at most 40 lines starting `+ ` or `- ` | Lines were added or removed |
| `no visible change` | Nothing changed |
| `dialog: <type> "<message>" (dismissed)` | A JavaScript dialog opened; the tool dismisses every dialog |
| `console: <n> new error(s); call console` | Console errors or exceptions arrived |

When the action loaded a new document, the full snapshot of that document replaces the comparison.

### Console and network

```text
[error] boom-load (http://127.0.0.1:8000/:4)
[exception] Uncaught TypeError: Cannot read properties of undefined (reading 'items') (file:///…/index.html:11)
[404] GET http://127.0.0.1:8000/missing.js
[failed] GET http://example.invalid/app.js — net::ERR_NAME_NOT_RESOLVED
```

The console keeps at most 500 entries and the network log at most 200, each cut at 2 000
characters. When older entries were dropped, the next read starts with
`… <n> earlier entries dropped`.

### Screenshots

A screenshot is saved as `.tool-browser/<session-id>/screenshot-<n>.png` in the work folder: the
1280×800 viewport, the whole page with `full_page`, or one element's box with `ref`. The result
names the file in `data` and lists it in `attachments`. A `mur` built from murmur `e25182ae` or later
shows the image to the model. An older `mur` passes on the path only, so the model sees the text
`saved …` and no image.

The first result after a browser starts begins with one line naming it:

```text
browser: chromium 145.0.7632.6, local, headless; Chrome sandbox off (murmur's seccomp filter is on, so the capsule's containment holds the browser)
```

## What the capsule must grant

Grant one of `chromium`, `chromium-browser`, `google-chrome` or `chrome` in
`capabilities.shell.allow`. The tool tries them in that order, takes the granted ones, and runs the
first that is on the `PATH` `mur run` starts with. On macOS, a granted name that is not on `PATH`
falls back to `/Applications/Google Chrome.app/Contents/MacOS/Google Chrome` (`google-chrome`,
`chrome`) or `/Applications/Chromium.app/Contents/MacOS/Chromium` (`chromium`).

| The capsule… | The call answers |
|---|---|
| Grants none of the four | `tool-browser needs a browser: add one of chromium, chromium-browser, google-chrome or chrome to capabilities.shell.allow` |
| Grants a name that is not on `PATH` | `tool-browser: capabilities.shell.allow grants chromium, but no chromium is on PATH; install Chrome or Chromium on this host first (mur deploy run does not upload host programs)` |

The tool never uses your own Chrome profile: each browser gets an empty profile in the work folder.

### Which browser runs on which tier

Measured on Ubuntu 24.04.5, Linux 7.0.0-28-generic, x86_64, with `mur` 0.9.0-dev at murmur
`0708dd31`.

| Browser | Full and Sealed | Seccomp-only, Environment-only (macOS) |
|---|---|---|
| `chrome-headless-shell` (Chrome for Testing 145) | Runs, with the grants below | Not measured |
| Google Chrome 156, `/opt/google/chrome/chrome` | Does not start | Runs outside a capsule; `cargo test -p tool-browser` uses it |
| Google Chrome's `google-chrome` wrapper script | Does not start | Runs outside a capsule |
| Ubuntu's snap `chromium-browser` | Not measured: no snap on the measuring host. It is full Chromium behind a snap launcher, so it meets the ProcessSingleton refusal below even if the launcher starts | Not measured |
| Debian's `chromium` wrapper | Not measured: not installed on the measuring host | Not measured |

Full Chrome and full Chromium bind a unix socket file for their ProcessSingleton when they start.
murmur's Landlock ruleset withholds the right to create a socket file on every path, the work
folder included, and no capsule declaration grants it. Chrome then stops:

```text
ERROR:chrome/browser/process_singleton_posix.cc:1083] Failed to bind() /tmp/com.google.Chrome.6850Jr/SingletonSocket: Permission denied (13)
ERROR:chrome/app/chrome_main_delegate.cc:624] Failed to create a ProcessSingleton for your profile directory. … Aborting now to avoid profile corruption.
```

and exits with status 21. `capabilities.network.unix_sockets: true` does not help, because it
lifts the seccomp refusal of `socket(AF_UNIX)` and not the Landlock refusal of the socket file.
`chrome-headless-shell` has no ProcessSingleton, so it is the browser for the Full and Sealed tiers.

### A working manifest on Full and Sealed

Install `chrome-headless-shell`, for example with `npx @puppeteer/browsers install chrome-headless-shell@stable`
or as Playwright's headless shell (the measurements used Playwright's build 1208). Then link it as
`chromium` **inside its own directory** and put that directory on `PATH`:

```bash
HS=$HOME/.cache/ms-playwright/chromium_headless_shell-1208/chrome-headless-shell-linux64
ln -s chrome-headless-shell "$HS/chromium"
export PATH="$HS:$PATH"   # in the shell that runs `mur run`
```

On Sealed the composed root binds the directory the granted binary resolves to, so a link kept in
another directory is not on `PATH` inside it. On Full a link anywhere on `PATH` works.

```yaml
capabilities:
  shell:
    allow:
      - chromium
    interpreter_runtime:
      - binary: chromium
        dirs:
          - path: /home/you/.cache/ms-playwright/chromium_headless_shell-1208/chrome-headless-shell-linux64
            list_dir: true
          - path: /proc
            list_dir: true
          - path: /etc/fonts
            list_dir: true
          - path: /usr/share/fonts
            list_dir: true
  resources:
    memory_bytes: 274877906944
    cgroup_pids_max: 1024
    cgroup_memory_bytes: 4294967296
```

| Grant | Without it |
|---|---|
| The browser's own directory | `Invalid file descriptor to ICU data received`, and the browser dies with SIGTRAP: it reads `icudtl.dat` and its `.pak` files from there |
| `/proc` | `GPU process launch failed: error_code=1002`, then a fatal GPU process error |
| `/etc/fonts`, `/usr/share/fonts` | The browser starts, but the page never renders and the first snapshot times out |
| `memory_bytes` | The browser dies at start; see [Resources](#resources) |

`interpreter_runtime` fires `W-SEC-009` at launch, because the directories tie the capsule to this
host's layout.

### The `google-chrome` wrapper on Full

`/usr/bin/google-chrome` is a bash script that runs `/opt/google/chrome/chrome`. Getting it as far
as Chrome itself took these grants, measured one refusal at a time:

| Refusal | Grant that cleared it |
|---|---|
| `cannot run /usr/bin/google-chrome: Permission denied (os error 13)` | `bash` in `shell.allow` (the script's `#!` interpreter) |
| `/usr/bin/readlink: Permission denied`, then `dirname`, `mkdir`, `touch`, `cat` | each of them in `shell.allow` |
| `error while loading shared libraries: libdl.so.2` | `interpreter_runtime` dirs `/opt/google/chrome` and `/usr/lib/x86_64-linux-gnu`: murmur derives a shared-library closure only for a granted ELF, not for what a script runs |
| Reads under `/proc` refused, then a fatal sandbox check | `/proc`, and the tool turning Chrome's sandbox off (below) |

Chrome then stops on the ProcessSingleton socket as above, so no manifest makes the wrapper work
on Full or Sealed. Granting `/opt/google/chrome/chrome` directly under the name `chrome` gets the
same result.

### Chrome's sandbox per tier

| Tier | `/proc/self/status` | Chrome's sandbox |
|---|---|---|
| Full, Sealed | `Seccomp: 2`, or unreadable because Landlock refuses it | Off: `--no-sandbox --disable-dev-shm-usage` |
| Seccomp-only | `Seccomp: 2` | Off |
| Environment-only (macOS, or Linux without kernel confinement) | No filter | On |

murmur's seccomp filter refuses the `unshare` and `chroot` calls Chrome's sandbox needs, Sealed
has no `/dev/shm`, and Landlock refuses writes there. So wherever the filter is on, the browser
runs without its own sandbox and the capsule's containment holds it instead: the session's network
namespace, the seccomp filter, and on Full and Sealed, Landlock.

A Linux host without Landlock is usually on the Seccomp-only tier, where the filter still applies,
so Chrome's sandbox is off there too. Only a tier with no seccomp filter keeps it on.

## Resources

Measured on the Full tier, for `chrome-headless-shell` 145 with one page open:

| Measure | Value |
|---|---|
| Smallest `memory_bytes` at which the browser started | 80 GiB (72 GiB failed). Google Chrome 156 needed 104 GiB outside a capsule (96 GiB failed) |
| Largest `VmPeak` of a browser process, under `memory_bytes` 256 GiB | 196 GiB, a renderer. Unlimited, a renderer reserves about 1.4 TiB |
| Total RSS of the tree | 440 MiB, shared pages counted once per process |
| cgroup `memory.current` | 71 MiB |
| Tasks in the tree | 62, threads included; this is what `cgroup_pids_max` counts |

`memory_bytes` is `RLIMIT_AS`, which counts address space reserved rather than memory used, and V8
reserves tens of gigabytes per process up front. The default 2 GiB stops every Chrome at start,
with an error line that names `capabilities.resources.memory_bytes`.

| Field | Default | Set | Why |
|---|---|---|---|
| `memory_bytes` | 2 GiB | `274877906944` (256 GiB) | Over three times the measured minimum. It is a per-process address-space ceiling, so real memory stays bounded by `cgroup_memory_bytes` |
| `cgroup_pids_max` | 256 | `1024` | 62 tasks for one page; each further page adds a renderer with about 10 threads |
| `cgroup_memory_bytes` | 4 GiB | `4294967296` (4 GiB) | The default holds a few pages comfortably; raise it for heavy pages |

## A laptop and a VM

The tool runs the browser on the host the capsule runs on. `mur deploy run` uploads no host
programs, so on a VM Chrome or Chromium must be installed there first. A VM without it answers the
first browser call with the not-on-`PATH` line above, which names the missing program.

## A browser elsewhere

```yaml
artifacts:
  - name: murmur/tool-browser
    version: 0.1.0
    runtime: tool
    config:
      endpoint: ws://browser.internal:9222/devtools/browser/0b9c…
capabilities:
  network:
    allow:
      - browser.internal
```

With `endpoint`, the tool connects to that DevTools endpoint instead of starting a browser, and
needs no `shell.allow` grant. `endpoint` is the only key and must start with `ws://` or `wss://`. A
`ws://host:port` with no path is looked up at `http://host:port/json/version`. The host must be in
`capabilities.network.allow`; when it cannot be reached, the error names it.

The remote browser cannot see the work folder, so `open` with a work-folder path is refused.
`close` and the idle stop close only the pages this session opened, then disconnect.

`config:` reaches a native tool only on a `mur` built from murmur `9c6bd749` or later. An older
`mur` does not pass it on, and the tool starts a local browser as if there were no `config:`.

## Files in the work folder

Everything lives in `.tool-browser/<session-id>/`, which is worth adding to `.gitignore`:

| Path | What |
|---|---|
| `profile/` | The browser's profile: created empty when a local browser starts, removed when it stops |
| `supervisor.json` | Mode 0600: the supervisor's pid, its loopback port and token, the browser's program, version and pid, and when it started |
| `supervisor.log` | The supervisor's and the browser's stderr, cut at 1 MiB |
| `screenshot-<n>.png` | Screenshots |
| `downloads/` | Files the page downloaded |

On Linux murmur kills every process of a session when it ends, so `profile/` and
`supervisor.json` of an ended session stay behind. The next browser started in the same work folder
removes them.

## Lifetime

| Event | What happens |
|---|---|
| Capsule launch | Nothing |
| First browser call | The tool checks the grant, starts a supervisor and the browser, and answers with the banner |
| Each later call | Served by the same browser, one call at a time, each bounded at 90 s |
| `close` | The browser stops; the next call starts a new one |
| 15 minutes without a call | The browser stops |
| Session end | murmur kills the browser on Linux. On macOS the supervisor sees the session's runtime directory go and stops the browser within 5 s |
