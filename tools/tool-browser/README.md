# tool-browser

Drives a headless Chrome or Chromium from a capsule, one action per tool call: open a page, read it
as text, click, type, press keys, choose options, wait, go back, read the console and failed
requests, and save screenshots.

The browser is a Chrome or Chromium on the host the capsule runs on: the host's own `chromium`, or
the `chrome-headless-shell` tool-browser pins for `chromium`, which `mur install` fetches. The
first browser call starts it as a session process through `murmur:process`, later calls reuse it,
and it runs until `close` or the session's end, when murmur kills it with its process group.
Nothing runs at capsule launch.

```bash
mur install -g murmur/tool-browser@0.5.0
```

`mur install` also fetches the pinned `chrome-headless-shell` for `chromium`, about 128 MB, once
per host, and checks its SHA-256:

```text
  fetched chromium for linux-x86_64 (murmur/tool-browser): 128 MB, sha256 verified
```

A later `mur install` on the same host prints
`chromium for linux-x86_64 (murmur/tool-browser): already in the store` and fetches nothing.

The tool is a WASM component that imports `murmur:process`. It needs a `mur` whose `mur version`
reports WIT level 4 or higher: murmur v0.10.0 is the first release that serves level 4.
An older `mur` installs the zip, then refuses the capsule at launch:

```text
error[E-WIT-LEVEL-001]: artifact 'local/tool-browser@0.5.0' does not fit WIT level 3, the level this mur serves: it imports murmur:process/process@0.1.0, and level 3 has no version of murmur:process
```

What Chrome needs from the host, and the pinned download, are declared in the tool's own
`programs:` block (see [What the capsule must grant](#what-the-capsule-must-grant)). murmur
`b0d4c42e` (#406) is the first `mur` that reads `programs.chromium.download`, and no murmur
release holds it yet. Older `mur`s:

| `mur` | What it does with this manifest |
|---|---|
| v0.10.0 and earlier | Ignores `programs:`: nothing is fetched, the browser's folder is not granted, and Chrome dies at start with `Invalid file descriptor to ICU data received` |
| A dev `mur` from murmur `38988719` up to the commit before `b0d4c42e` | Refuses the manifest with `E-RUN-062` at launch (`E-BLD-008` at build), naming `programs.chromium.download`: `is not a key of a program; a program holds needs` |

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
`click needs ref (a ref such as e3 from a snapshot)`. The tool takes no `config:` keys: an entry
that sets any answers `tool-browser: config: does not take <key>; tool-browser takes no config: keys`.

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
  number. A new document starts again at `e1`. Refs live in `state.json`, so they hold across calls.
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
| `dialog: <type> "<message>" (dismissed)` | A JavaScript dialog opened, during a call or between calls; the tool dismisses every dialog |
| `dialog: (dismissed)` | A dialog was open when the call began, and the tool learned nothing more about it |
| `console: <n> new error(s); call console` | Console errors, failed assertions, exceptions or error-level log entries arrived |

When the action loaded a new document, the full snapshot of that document replaces the comparison.

A dialog that opens while no call is running would block the page in full Chrome, and no later
DevTools session can dismiss it there. So each call replaces the page's `alert`, `confirm` and
`prompt` with stand-ins that record the dialog and return what dismissing it returns: nothing,
`false` and `null`. The next call reports what they recorded.

### Console and network

```text
[error] boom-load (http://127.0.0.1:8000/:4)
[exception] Uncaught TypeError: Cannot read properties of undefined (reading 'items') (file:///…/index.html:11)
[404] GET http://127.0.0.1:8000/missing.js
[failed] GET http://example.invalid/app.js — net::ERR_NAME_NOT_RESOLVED
[500] http://127.0.0.1:8000/api/items
[failed] http://127.0.0.1:9/x.png — net::ERR_CONNECTION_REFUSED
```

Each call opens a new DevTools session, and Chrome replays the current document's stored console
messages, exceptions and log entries to every new session. The tool keeps a fingerprint of every
entry it has taken in `state.json`, so each entry is reported once, whether it arrived during a call
or between calls.

A request that fails during a call is listed as `[<status>] <METHOD> <url>` or
`[failed] <METHOD> <url> — <error>`. One that fails between calls reaches the tool only through
Chrome's log, which carries no method, so it is listed as `[<status>] <url>` or
`[failed] <url> — <net::ERR_…>`. Either way it is also in the console, as Chrome's
`Failed to load resource: …` line.

The console keeps at most 500 unread entries and the network log at most 200, each cut at 2 000
characters. When older entries were dropped, the next read starts with
`… <n> earlier entries dropped`.

### Screenshots

A screenshot is saved as `.tool-browser/<id>/screenshot-<n>.png` in the work folder: the 1280×800
viewport, the whole page with `full_page`, or one element's box with `ref`. The result names the
file in `data` and lists it in `attachments`, and murmur shows the image to the model.

The first result after a browser starts begins with one line naming it:

```text
browser: chromium 145.0.7632.6, local, headless; Chrome sandbox off (Chrome could not start its sandbox inside this capsule's containment, which holds the browser instead)
```

## What the capsule must grant

Grant one of `chromium`, `chromium-browser`, `google-chrome` or `chrome` in
`capabilities.shell.allow`. The tool reads which ones are granted from the manifests murmur stages
in the session's runtime directory. It tries them in that order, `chromium` first, and starts the
first one that runs. A program is always the bare granted name: to run a browser that is not on
`PATH`, such as a macOS app bundle, put a link with the granted name on `PATH`.

| The capsule… | The call answers |
|---|---|
| Grants none of the four | `tool-browser needs a browser: add one of chromium, chromium-browser, google-chrome or chrome to capabilities.shell.allow` |
| Grants `chromium`, and neither the host nor the store has a copy | `tool-browser: capabilities.shell.allow grants chromium, but no chromium is on PATH and no pinned copy is in place; run mur install in the capsule's project to fetch the chrome-headless-shell tool-browser pins for chromium, or install Chrome or Chromium on this host; mur doctor names what is missing` |
| Grants only other names, none on `PATH` | `tool-browser: capabilities.shell.allow grants google-chrome, but no google-chrome is on PATH; grant chromium, for which tool-browser pins a chrome-headless-shell that mur install fetches, or install Chrome or Chromium on this host` |
| Declares `capabilities.filesystem.scope` on the tool's entry | `tool-browser cannot see which programs capabilities.shell.allow grants, because its entry declares capabilities.filesystem.scope; tool-browser runs only from an entry without one`. Such an entry gets no runtime directory |
| Refuses the start by a rule, such as an `on-shell` hook | `tool-browser: murmur refused to start chromium: <rule>: <reason>` |

The tool never uses your own Chrome profile: each browser gets an empty profile in the work folder.

### Which copy runs

Only `chromium` has a pinned download. For a capsule that grants it:

| When | The `chromium` that runs | `mur doctor` and `--explain-scope` say |
|---|---|---|
| A `chromium` is on the host's `PATH` | The host's | `runs <path> (host)`, `host <path>` |
| Otherwise, the capsule pins tool-browser with `mur install` and this platform is pinned (see [Pinned build](#pinned-build)) | The store's copy, `~/.murmur/programs/<sha256>/` | `runs <path> (pinned)`, `pinned <path> (sha256 …, from <url>)` |
| Otherwise | None: the call answers the not-on-`PATH` line above | `warning[W-RUN-012]: <artifact> pins chromium for <platform>, and the store holds no copy; run mur install` |

murmur grants the store's copy and its declared needs, though the store lies under the home
folder. A tool installed by a pull at run time gets no download and no grant.

`chromium` is tried before `chromium-browser`, `google-chrome` and `chrome`. So a capsule that
grants `chromium` and another name, on a host whose only browser is the other one, starts the
pinned headless shell. A capsule that wants the host's own Chrome grants only that name.

### Which browser runs on which tier

Measured on Ubuntu 24.04.5, Linux 7.0.0-28-generic, x86_64. The pinned `chrome-headless-shell`
157.0.8103.38 and Playwright's `chrome-headless-shell` 145.0.7632.6 (build 1208) with `mur`
0.11.0-dev at murmur `b0d4c42e`, opening a work-folder page and `https://example.com/` on both
tiers; the full browsers with `mur` 0.10.0-dev at murmur `6750b5f9`.

| Browser | `scoped` and `sealed` | Outside a capsule |
|---|---|---|
| The pinned `chrome-headless-shell` 157.0.8103.38, from the store | Runs, with what tool-browser declares | Not measured |
| `chrome-headless-shell` (Chrome for Testing 145) on `PATH` as `chromium` | Runs, with what tool-browser declares | Runs |
| Google Chrome 156, `/opt/google/chrome/chrome` or its `google-chrome` wrapper | Does not start | Runs; `cargo test -p tool-browser` uses it |
| Playwright's full Chromium 1208 | Does not start | Runs |
| Ubuntu's snap `chromium-browser`, Debian's `chromium` wrapper | Not measured: neither is installed on the measuring host. Both are full Chromium, so they meet the refusal below | Not measured |

Full Chrome and full Chromium create a unix socket for their ProcessSingleton when they start.
murmur's seccomp filter refuses `socket(AF_UNIX)` unless `capabilities.network.unix_sockets` is
declared, and Landlock withholds the right to create the socket file on every path, so Chrome stops:

```text
FATAL:chrome/browser/process_singleton_posix.cc:297] Check failed: . socket() failed: Permission denied (13)
```

On an earlier `mur`, the same refusal surfaced as `Failed to bind() …/SingletonSocket: Permission
denied (13)`, followed by an exit with status 21. Either way the call answers
`tool-browser: chromium exited with <code> while starting: <Chrome's FATAL line>; full Chrome and
Chromium cannot run on murmur's scoped and sealed tiers, grant chrome-headless-shell under this name
instead`. `chrome-headless-shell` has no ProcessSingleton, so it is the browser for those tiers.

### A working manifest on `scoped` and `sealed`

```yaml
artifacts:
  - name: murmur/tool-browser
    version: 0.5.0
    runtime: tool
capabilities:
  containment: sealed  # or scoped
  shell:
    allow: [chromium]
  network:
    allow: [https://example.com]  # the hosts the task opens
  resources:
    memory_bytes: 274877906944
    cgroup_pids_max: 1024
    cgroup_memory_bytes: 4294967296
```

In the capsule's project, on a host with no browser:

```bash
mur install
```

It installs tool-browser and fetches the pinned `chrome-headless-shell` for `chromium`, and the
capsule runs that copy.

The capsule names no host path. tool-browser's `murmur.yaml` declares, under `programs:`, what
Chrome needs from the host, by name, and the same list for each of `chromium`, `chromium-browser`,
`google-chrome` and `chrome`. murmur grants it, read-only, when both hold:

- the capsule pins the tool with `mur install`, not a pull at run time
- the capsule grants the name in `capabilities.shell.allow`

`mur run --explain-scope` lists what it granted under `declared needs:`, one line per need, such as
`local/tool-browser (chromium): file libsoftokn3.chk beside libsoftokn3.so -> /usr/lib/x86_64-linux-gnu/libsoftokn3.chk`.
`mur doctor` names a need this host lacks, with the Ubuntu package that provides it. For the
pinned copy, the needs resolve against the copy, so `program_folder .` is its folder in the store.

| What tool-browser declares | Without it |
|---|---|
| The browser's own folder (`program_folder: .`) | `Invalid file descriptor to ICU data received`, and the browser dies with SIGTRAP: it reads `icudtl.dat` and its `.pak` files from there |
| NSS's modules, which Chrome loads with `dlopen`: `libsoftokn3.so`, `libfreebl3.so`, `libfreeblpriv3.so`, `libnssdbm3.so` and `libnssckbi.so`, with the `.chk` files beside them | The first `https` page kills the browser with exit code 133. Chrome prints `ERROR:crypto/nss_util.cc:225] Error initializing NSS with a persistent database (…): libsoftokn3.so: cannot open shared object file: No such file or directory`, then `FATAL:crypto/nss_util.cc:115] nss_error=-5925, os_error=0`, and the call answers `tool-browser: chromium exited with 133 during Page.navigate: <that FATAL line>; Chrome loads NSS's modules with dlopen: tool-browser declares them for chromium under programs: …`. They are not in the shared-library closure murmur derives from the granted binary, and each `.chk` is its module's integrity file, which NSS reads beside it. `library: libsoftokn3.so` also brings the `libsqlite3.so.0` it links |
| The fonts (`system: fonts`, `etc: fonts/fonts.conf`) | The browser starts and the snapshot reads the page's text, but no text is drawn: a screenshot shows the page without a glyph, and `browser.log` says `Fontconfig error: Cannot load default config file: Unable to open /etc/fonts/fonts.conf` |
| The libraries Chrome links, such as `libnss3.so`, `libgbm.so.1` and `libasound.so.2` | murmur grants them anyway, as the browser's own shared libraries; the declaration lets `mur doctor` name the package when one is missing |

| The capsule still sets | Without it |
|---|---|
| `memory_bytes` | The browser dies at start; see [Resources](#resources) |
| `network.allow` | `open https://<host>/ failed: net::ERR_NAME_NOT_RESOLVED; if <host> is outside the capsule, add it to capabilities.network.allow`. Every `https` host a page loads from must be listed |

The tool adds no launch flag for the capsule. Each of `--disable-software-rasterizer`,
`--use-gl=disabled`, `--use-angle=swiftshader`, `--in-process-gpu` and `--disable-features=Vulkan`
was tried and none was needed: Chrome's GPU process starts and stays up on both tiers with what
tool-browser declares.

### Preparing a host

`mur install` in the capsule's project fetches the browser. The pinned zip does not carry the
system libraries Chrome links, NSS's modules or the fonts, so on Ubuntu 24.04 a host needs these
packages:

```bash
sudo apt-get install -y unzip fontconfig fonts-liberation fonts-noto-core libnss3 libnspr4 libatk1.0-0t64 libatk-bridge2.0-0t64 libxcomposite1 libxdamage1 libxfixes3 libxrandr2 libgbm1 libasound2t64 libatspi2.0-0t64
```

`mur doctor` names any that is missing.

### Pinned build

`programs.chromium.download` pins Chrome for Testing's `chrome-headless-shell`, one build for every
platform:

| Platform | Chrome for Testing build | Measured |
|---|---|---|
| `linux-x86_64` | 157.0.8103.38 (`linux64`) | Yes, on `scoped` and `sealed` |
| `linux-aarch64` | 157.0.8103.38 (`linux-arm64`) | No: pinned and checked, not run under containment |
| `darwin-aarch64` | 157.0.8103.38 (`mac-arm64`) | No: pinned and checked, not run under containment |
| Any other | None | No download: a host program named `chromium` runs |

The copy comes from Google's Chrome for Testing bucket, `storage.googleapis.com/chrome-for-testing-public`,
under Google's terms; the zip's `ABOUT` points at Chrome's Terms of Service. Neither tool-browser
nor mur keeps a copy. `mur install` stores it in `~/.murmur/programs/<sha256>/`, where every project
on the host shares it, and only a person removes it. `mur doctor` re-verifies it.

### Keeping the pinned build current

1. Read Stable's `chrome-headless-shell` URLs from
   `https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json`,
   under `channels.Stable.downloads["chrome-headless-shell"]`.
2. For each of `linux64`, `linux-arm64` and `mac-arm64`, in a new, empty folder:

   ```bash
   curl -fsSLO https://storage.googleapis.com/chrome-for-testing-public/<build>/<cft>/chrome-headless-shell-<cft>.zip
   sha256sum chrome-headless-shell-<cft>.zip
   zipinfo chrome-headless-shell-<cft>.zip | grep -c '^l'   # symlinks: must print 0
   ```

3. In one edit of `murmur.yaml`, replace the build in all three `url`s and the three `sha256`s.
4. Bump tool-browser's minor version.
5. Run `cargo test -p tool-browser --test manifest`. It checks that every entry uses one build and
   that this README names it, so update the build in [Pinned build](#pinned-build) and in
   [Which browser runs on which tier](#which-browser-runs-on-which-tier).
6. Run the `mur_run_browser` test (see BUILD.md).

### Your own browser instead

A `chromium` on the host's `PATH` runs instead of the pinned copy. A full Chromium named `chromium`
takes precedence too, and it does not start on `scoped` or `sealed` (see the ProcessSingleton
refusal above). To run a `chrome-headless-shell` of your own, keep it **outside the home folder**:
murmur grants no declared need of a host program that resolves beneath the `HOME` of the `mur`
that runs the capsule. `npx @puppeteer/browsers` and Playwright install under `~/.cache` by
default, so install `chrome-headless-shell` elsewhere, link it as `chromium` **inside its own
folder**, and put that folder first on `PATH`:

```bash
sudo npx @puppeteer/browsers install chrome-headless-shell@stable --path /opt/chrome-headless-shell
HS=$(dirname /opt/chrome-headless-shell/chrome-headless-shell/linux-*/chrome-headless-shell-linux64/chrome-headless-shell)
sudo ln -s chrome-headless-shell "$HS/chromium"
export PATH="$HS:$PATH"   # in the shell that runs `mur run`
```

On `sealed` the composed root binds the folder the granted binary resolves to, so a link kept in
another folder is not on `PATH` inside it. On `scoped` a link anywhere on `PATH` works.

A browser under `$HOME` gets this from `mur doctor`, and dies at start with the ICU line above,
because its folder is not granted:

```text
warning[W-RUN-011]: local/tool-browser declares program_folder . for chromium, and it is not granted: resolved to /home/you/.cache/ms-playwright/chromium_headless_shell-1208/chrome-headless-shell-linux64, beneath the home folder /home/you, which no need reaches
```

### The `google-chrome` wrapper on `scoped`

`/usr/bin/google-chrome` is a bash script that runs `/opt/google/chrome/chrome`. Getting it as far
as Chrome itself took these grants, measured one refusal at a time with `mur` at murmur `0708dd31`:

| Refusal | Grant that cleared it |
|---|---|
| `Permission denied (os error 13)` at the start | `bash` in `shell.allow` (the script's `#!` interpreter); the tool's line says so |
| `/usr/bin/readlink: Permission denied`, then `dirname`, `mkdir`, `touch`, `cat` | each of them in `shell.allow` |
| `error while loading shared libraries: libdl.so.2` | `interpreter_runtime` dirs `/opt/google/chrome` and `/usr/lib/x86_64-linux-gnu`: murmur derives a shared-library closure only for a granted ELF, not for what a script runs |
| Reads under `/proc` refused, then a fatal sandbox check | `/proc`, and Chrome's sandbox off (below) |

Chrome then stops on the ProcessSingleton socket as above, so no manifest makes the wrapper work
on `scoped` or `sealed`. For a capsule granting `google-chrome`, the declared `program_folder: .`
resolves to `/opt/google/chrome`: `/usr/bin/google-chrome` is a link to
`/opt/google/chrome/google-chrome`, and that folder holds both the wrapper and Chrome, with its
`icudtl.dat`. `mur run --explain-scope` at murmur `b0d4c42e` shows
`program_folder . -> /opt/google/chrome (list_dir)`.

### Chrome's sandbox

A WASM tool cannot read `/proc/self/status`, so the tool decides by observation. The first start
runs with Chrome's own sandbox. If Chrome exits while starting and its stderr shows a sandbox
failure, the tool starts it once more with `--no-sandbox`, and the banner says which one runs:

| Banner ends with | When |
|---|---|
| `Chrome sandbox on` | Chrome started its sandbox: outside a capsule, or on a tier with no seccomp filter |
| `Chrome sandbox off (Chrome could not start its sandbox inside this capsule's containment, which holds the browser instead)` | Measured on `scoped` and `sealed`, where Chrome prints `FATAL:content/browser/zygote_host/zygote_host_impl_linux.cc:128] No usable sandbox!` |

murmur's seccomp filter refuses the `unshare` call Chrome's sandbox needs, and `no_new_privs` rules
out its setuid helper. Inside a capsule the browser therefore runs without its own sandbox, and the
capsule's containment holds it: the session's network namespace, the seccomp filter and Landlock,
plus the composed root on `sealed`. murmur's trace shows both starts, a `process_exit` with code 133
for the first.

## Resources

Measured on `scoped` with `mur` at murmur `0708dd31`, for `chrome-headless-shell` 145 with one page
open. A re-check at `6750b5f9` matched: the browser fails to start at 8 GiB and 32 GiB, and runs at
80, 96 and 256 GiB.

| Measure | Value |
|---|---|
| Smallest `memory_bytes` at which the browser started | 80 GiB (72 GiB failed). Google Chrome 156 needed 104 GiB outside a capsule (96 GiB failed) |
| Largest `VmPeak` of a browser process, under `memory_bytes` 256 GiB | 196 GiB, a renderer. Unlimited, a renderer reserves about 1.4 TiB |
| Total RSS of the tree | 440 MiB, shared pages counted once per process |
| cgroup `memory.current` | 71 MiB |
| Tasks in the tree | 62, threads included; this is what `cgroup_pids_max` counts |

`memory_bytes` is `RLIMIT_AS`, which counts address space reserved rather than memory used, and V8
reserves tens of gigabytes per process up front. Under the default 2 GiB, Chrome exits at start
with nothing on stderr. The call then answers `tool-browser: chromium exited with 133 while starting:
(nothing on stderr); set capabilities.resources.memory_bytes to at least 85899345920 (80 GiB), for
example 274877906944`.

| Field | Default | Set | Why |
|---|---|---|---|
| `memory_bytes` | 2 GiB | `274877906944` (256 GiB) | Over three times the measured minimum. It is a per-process address-space ceiling, so real memory stays bounded by `cgroup_memory_bytes` |
| `cgroup_pids_max` | 256 | `1024` | 62 tasks for one page, and each further page adds a renderer with about 10 threads. At 32, `pthread_create` fails and every call answers `the page does not answer within 10 s …` |
| `cgroup_memory_bytes` | 4 GiB | `4294967296` (4 GiB) | The default holds a few pages comfortably; raise it for heavy pages |

## A laptop and a VM

The tool runs the browser on the host the capsule runs on. `mur install` fetches the pinned copy on
each host, and `mur deploy run` uploads no host programs, so on a VM run `mur install` there, or
install a browser there. A VM with neither answers the first browser call with the not-on-`PATH`
line above, which says how to get a copy.

## Files in the work folder

Each browser started gets its own folder, `.tool-browser/<id>/`, where `<id>` is 16 random hex
characters. `.tool-browser/` is worth adding to `.gitignore`:

| Path | What |
|---|---|
| `profile/` | The browser's profile: created empty when the browser starts, removed by `close` |
| `downloads/` | Files the page downloaded |
| `blank.html` | The page Chrome opens first; its URL tells the tool where the work folder is |
| `state.json` | The session-process handle, Chrome's DevTools port and path, the work folder's `file://` URL, the page's target id, refs, the last snapshot, unread console and network entries with their fingerprints, and the screenshot counter |
| `browser.log` | Chrome's stdout and stderr, cut at 1 MiB with one closing line |
| `call.lock` | Held while a call runs, so calls take turns, in no set order. The call that starts the browser takes it before Chrome starts, so its action runs first. A call waits up to 90 s for it, then answers `tool-browser: another tool-browser call is still running`. A lock older than 120 s was left by a call that died, and the next call takes it over |
| `screenshot-<n>.png` | Screenshots |

A session that ends without `close` leaves its folder, profile included. A WASM tool cannot tell a
folder of another live session from one of an ended session, so the tool removes none. Delete
`.tool-browser/` when no session runs in the work folder.

## Lifetime

| Event | What happens |
|---|---|
| Capsule launch | Nothing |
| First browser call | The tool checks the grant, starts Chrome as the session process `browser` and answers with the banner |
| Each later call | The same Chrome, one call at a time; calls that arrive while it is starting wait for that start, then take turns in no set order. Each opens its own DevTools connection, which closes when the call returns. A call is bounded at 90 s, counted from when it began, including any wait for another call to finish; a call that reaches it, a long `wait` included, answers `tool-browser: the call ran out of its 90 s` |
| `close` | Chrome stops and its profile is removed; the next call starts a new one |
| Chrome exits on its own | A call it exits during answers `tool-browser: <name> exited with <code> during <the DevTools method in flight, else the action>: <Chrome's last FATAL line, else its last line>`, plus a hint when the cause is known. The next call starts a new one, and its first result says so: `browser: the previous browser exited with <code>; this is a new one and refs start again at e1` |
| Session end | murmur kills Chrome with its process group, on every platform, and the trace records `process_exit` with `reason: session_end` |

Calls sent together share one browser. A harness on the process transport may send all of a
turn's calls at once; they find the browser through `murmur:process`, wait for the one start, and
then run one at a time in no set order, with the starting call's action first. A model that needs
an order sends each call after the last one answers.

A start has 30 s to bring Chrome up, through the sandbox retry, and record it in `state.json`:

| When | The call answers |
|---|---|
| The starting call runs out of the 30 s | `tool-browser: chromium did not finish starting within 30 s, so it was stopped; see .tool-browser/<id>/browser.log` |
| A waiting call finds a running browser that no `state.json` records for 35 s, because the call starting it died | `tool-browser: the running browser (<handle>) has no state.json under .tool-browser/, so it was stopped; the next call starts a new one` |
| Chrome exits while starting, and other calls are waiting on that start | Each waiting call makes its own attempt about 1 s after the last one failed, and answers its own `tool-browser: chromium exited with <code> while starting: …` if that fails too |

## Changes in 0.5.0

- `murmur.yaml` pins Chrome for Testing's `chrome-headless-shell` 157.0.8103.38 as the download of
  `chromium` for `linux-x86_64`, `linux-aarch64` and `darwin-aarch64`. `mur install` fetches it
  once per host, and a capsule granting `chromium` runs it when no `chromium` is on `PATH`. It
  needs a `mur` at murmur `b0d4c42e` or later; a dev `mur` from `38988719` before that refuses the
  manifest with `E-RUN-062`.
- A capsule granting `chromium` and another name, on a host whose only browser is the other one,
  now starts the pinned headless shell. Grant only the other name to keep the host's browser.
- When no copy can run, the call says to run `mur install` in the capsule's project, or, for a
  capsule that grants only other names, that granting `chromium` gets the pinned copy.

## Changes in 0.4.0

- `murmur.yaml` declares what Chrome needs from the host under `programs:`, the same list for
  `chromium`, `chromium-browser`, `google-chrome` and `chrome`: the browser's folder, NSS's modules
  with their `.chk` files, the fonts, and the libraries Chrome links with their Ubuntu packages.
- The working manifest shrinks to the artifact, the `chromium` grant, `network.allow` and
  `resources`. It has no `interpreter_runtime` entry and no host path, so `W-SEC-009` no longer
  fires. It needs a `mur` at murmur `38988719` or later.
- The browser must live outside the home folder, because murmur grants no declared need beneath it.

## Changes in 0.3.0

- A browser that exits during a call answers `tool-browser: <name> exited with <code> during
  <method>: <Chrome's FATAL line>`, plus a hint when the cause is known, such as NSS's modules
  missing on the first `https` page.
- Calls sent together share one browser: they wait for the one start, then run one at a time in
  no set order, with the starting call's action first. A start that does not finish within 30 s is
  stopped and named.
- The working manifest grants NSS's modules and `network.allow`, so it opens `https` pages.

## Breaking in 0.2.0

- The tool is a WASM component, one zip for every platform. It needs a `mur` reporting WIT level 4
  or higher.
- `config.endpoint` is gone, and every `config:` key is refused. A browser running elsewhere can no
  longer be reached: a WASM tool reaches only programs it started.
- The macOS app-bundle lookup is gone. Put a link named after the granted name on `PATH`.
- There is no idle stop. The browser runs until `close` or the session's end.
- `supervisor.json` and `supervisor.log` are replaced by `state.json` and `browser.log`, in a
  per-browser `.tool-browser/<id>/` instead of `.tool-browser/<session-id>/`.
- Profiles of ended sessions are no longer swept.
- Chrome's sandbox is decided by observation, not by reading `/proc/self/status`.
- A request that fails between calls is listed without its method: `[<status>] <url>` or
  `[failed] <url> — <net::ERR_…>`.
