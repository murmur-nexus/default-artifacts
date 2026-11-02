# hook-command-guard

Refuses a shell command before it runs when the command matches a rule the operator wrote in
the hook's `config:` block: a denied program, a program with argument conditions such as
`rm -rf` or `git push --force`, or a regular expression. The hook reads the command the way a
shell would: it lexes scripts, walks every command in them, and looks through wrappers such as
`sudo`, `xargs` and `bash -c`. It has no built-in rules: with no config, or an empty one, it
allows every command.

WASM hook component (`runtime: hook`, exports `murmur:hook/lifecycle`).
Binding: `on-shell` · mode: blocking · commit policy: `deny`.

When a rule matches, the hook returns `deny(reason)`. The runtime refuses the call, tells the
agent which hook refused it and why, and records a `call_denied` line in `trace.jsonl`. The
command does not run. The hook holds no capability.

## Install

```sh
mur install -g murmur/hook-command-guard@0.1.0
```

Add the hook to the capsule's `artifacts:` list with a `config:` block. See
[Example config](#example-config).

## Config

The runtime delivers the `config:` block to the hook as compact JSON in
`MURMUR_ARTIFACT_CONFIG`.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `on_unclear` | `"deny"` \| `"allow"` | `"deny"` | What to do with a command the guard cannot read. See [Unclear commands](#unclear-commands). |
| `deny_programs` | list of program names | `[]` | Programs refused outright. |
| `rules` | list of rule objects | `[]` | A program plus argument conditions, each with a `reason`. |
| `patterns` | list of `{ pattern, reason }` | `[]` | Regular expressions for what the rules cannot express. |

**An empty config allows everything, unclear commands included.** `on_unclear` applies only
when at least one of `deny_programs`, `rules` or `patterns` is non-empty. An unset variable,
`{}` and `{"on_unclear": "deny"}` all allow `rm -rf /` and `eval "$X"`.

### Rule objects

| Key | Required | Type | Holds when |
|---|---|---|---|
| `program` | yes | name or list of names | the command's program matches one of them |
| `reason` | yes | non-empty string | — the text the agent sees |
| `name` | no | string | — quoted in the refusal beside `rules[<i>]` |
| `subcommand` | no | string or list | the subcommand is one of them |
| `all_flags` | no | list of flag specs | every spec is present |
| `any_flag` | no | list of flag specs | at least one spec is present |
| `path_is` | no | list of paths | some operand is exactly one of these paths |
| `path_under` | no | list of paths | some operand is one of these paths or lies beneath one |
| `any_arg` | no | list of regexes | some operand matches one of them |
| `unless` | no | list of flag specs | — when any spec is present, the rule does not apply |

A rule matches when every condition it states holds and no `unless` spec is present. A rule
with only `program` refuses every call of that program. Every key a rule states must be
non-empty.

### Program names

`deny_programs` entries and `program` names are exact basenames, case-sensitive. A trailing `*`
makes a prefix match: `mkfs.*` matches `mkfs.ext4`. A `*` anywhere else, a `/`, or an empty
name is a config error.

The program of a command is the basename of its first word, so `rm`, `\rm` and `/bin/rm` are
all `rm`.

### Flag specs

A flag spec is one spelling (`"-f"`) or a list of spellings (`["-r", "-R", "--recursive"]`).
The spec is present when any of its spellings is. Every spelling starts with `-` and has at
least two characters.

Flags are read from the command's literal words before a bare `--`:

| Word | Flags it gives |
|---|---|
| `--name`, `--name=value` | `--name` |
| `-abc` | `-a`, `-b`, `-c` and `-abc` itself |
| `-` | none; it is an operand |

So `-rf` satisfies both `-r` and `-f`, and `find … -delete` satisfies a `-delete` spec. A word
holding an expansion (`"$FLAGS"`) never satisfies or blocks a flag condition.

### Operands and subcommands

Operands are every word that is not a flag, plus every word after `--`. When a rule names a
`subcommand`, only the operands after the subcommand count.

The subcommand matches when it equals the first operand, or the second operand when the first
directly follows a flag word (and so could be that flag's value). This handles
`git -C dir push` and `git -c key=value push` without a table of each program's options.

### Paths

`path_is` and `path_under` compare operands with the configured paths lexically; the hook has
no filesystem access.

| Operand | Read as |
|---|---|
| `/…` | rooted; empty and `.` components dropped, `..` pops one |
| `~`, `~/…`, a leading `$HOME` or `${HOME}`, quoted or not | home-anchored; compares only with `~` paths in the config |
| anything else literal | relative; compares only with relative paths in the config |
| a relative path whose `..` rises above its start | not known until the command runs |
| a word with an unquoted `*`, `?` or `[` | the literal directory before its first glob component: `/*` is `/`, `~/*` is `~` |
| `~user`, or any other expansion | not known until the command runs |

Configured paths follow the same rules. `~user`, `$`, an empty path and a relative path that
rises above its start are config errors.

### `any_arg`

`any_arg` is the operand condition that flags and paths cannot express. Refusing
`git push origin main` and `git push origin HEAD:main` while allowing
`git push origin feature-main` needs an exact operand match, and so does `dd of=/dev/sda`,
whose operands are `key=value` words. The regexes are unanchored: write `^…$` to match a whole
operand.

### `patterns`

Each pattern is a regular expression searched for in these texts:

| Call | Text |
|---|---|
| a shell interpreter call | the `-c` script |
| a direct call | the reassembled command line: the program's basename, then each argument, separated by single spaces, with an argument holding whitespace or a shell metacharacter single-quoted |
| a `make`, `just` or `npm run` call | the recipe body, as well |

Regular expressions use the Rust `regex` syntax, compiled with a 1 MiB size limit. Matching
runs in linear time.

## Example config

```yaml
artifacts:
  - name: murmur/hook-command-guard
    version: 0.1.0
    runtime: hook
    config:
      on_unclear: deny
      deny_programs: [mkfs, "mkfs.*", mke2fs, wipefs]
      rules:
        - name: recursive-force-delete
          program: rm
          all_flags: [[-r, -R, --recursive], [-f, --force]]
          reason: "rm -rf deletes without asking. Delete the files you mean by name, or ask the operator."
        - name: delete-system-path
          program: rm
          any_flag: [[-r, -R, --recursive]]
          path_is: ["/", "~", /bin, /boot, /etc, /home, /lib, /opt, /usr, /var]
          reason: "A recursive delete of the root, the home folder or a system folder."
        - name: force-push
          program: git
          subcommand: push
          any_flag: [[-f, --force], --force-with-lease, --force-if-includes, --mirror]
          reason: "A force push rewrites the remote's history. Push without force, or ask the operator."
        - name: force-push-refspec
          program: git
          subcommand: push
          any_arg: ['^\+']
          reason: "A refspec starting with + force-pushes that branch."
        - name: push-to-main
          program: git
          subcommand: push
          any_arg: ['^\+?([^:]*:)?(refs/heads/)?(main|master)$']
          reason: "Pushing to main or master is the operator's step. Push a feature branch instead."
        - name: cargo-publish
          program: [cargo, rustup]   # a direct cargo call resolves to rustup on a rustup host
          subcommand: publish
          unless: [[--dry-run, -n]]
          reason: "Publishing a crate cannot be undone. Run cargo publish --dry-run instead."
        - name: dd-to-device
          program: dd
          any_arg: ['^of=/dev/(sd|hd|vd|xvd|nvme|mmcblk|disk|md|dm-|loop|mapper/)']
          reason: "dd onto a device overwrites the disk."
      patterns:
        - pattern: '(curl|wget)\b[^|;&\n]*\|\s*(sudo\s+(-\S+\s+)*)?(env\s+)?(ba|da|z|k)?sh\b'
          reason: "A download piped into a shell runs code nobody has read. Download it, read it, then run it."
```

Under this config `ls` runs, and `rm -rf ./x` is refused with:

```text
rules[0] (recursive-force-delete): rm -rf deletes without asking. Delete the files you mean by name, or ask the operator.; matched `rm -rf ./x`
```

`sudo rm -rf x`, `ls | xargs rm -rf` and `bash -c 'rm -rf x'` are refused by the same rule.

## What the guard reads

The guard decides on these fields of the `shell-event`:

| Field | How it is judged |
|---|---|
| `outcome` is set | Not judged: the call has already run. |
| `script` is set | The interpreter call itself is judged as a direct command, then the script is lexed and every command in it is judged. |
| `script` is not set | One direct command: the program is the basename of `binary`, its words are `argv`. Wrappers are looked through, so `binary` `/usr/bin/sudo` with `argv` `[rm, -rf, x]` is judged as `sudo` and as `rm`. No shell runs a direct call, so `~`, `$HOME` and globs in it are plain text. |
| `recipe` is set | Judged as well as the command. The body is prepared by kind, then lexed and walked like a script. A refusal from it ends with ` (in the recipe body this call runs)`. |
| `recipe` is not set | Nothing more. |
| `command` | Never read: it is clipped to 200 characters. |

In a script the guard walks pipelines, `&&`, `||`, `;`, `&`, newlines, subshells, `{ }` groups,
`if`/`while`/`until`/`for`/`select`/`case` bodies, function bodies, and command and process
substitutions. It skips assignments, reserved words, `for` headers, `case` patterns,
`[[ … ]]` and `(( … ))`, but still reads the substitutions inside them. A heredoc body is data:
it is never read as commands. A `#` at the start of a word begins a comment.

### Recipe kinds

The kind comes from the basename of `binary`.

| Kind | Basename | Preparation |
|---|---|---|
| make | starts with `make` or `gmake` | Each line loses its leading `@`, `-` and `+`. `$$` is `$`. `$(…)`, `${…}` and `$X` are make expansions whose value is not known; a command name built from one is unclear. |
| just | `just` | Each line loses its leading `@` and `-`. `{{…}}` is an interpolation whose value is not known. A body whose `#!` line names something other than a shell interpreter is unclear. |
| shell | anything else, `npm` and `npm-cli.js` included | None: an npm script is shell text. |

### Wrappers

Each layer is judged as its own command: both the wrapper and what it runs.

| Wrapper | Options skipped | Inner command |
|---|---|---|
| `sudo` | `-u -g -h -p -r -t -C -D -T -U` and their long forms take a value; other `-…` words; `NAME=value` words; `--` | the first remaining word; none for `-s` or `-i` alone, or `-e`/`--edit` |
| `env` | `-u`/`--unset` and `-C`/`--chdir` take a value; `-i -0 -v -`; `NAME=value` words. `-S`/`--split-string VALUE` is split into words that start the inner command. | the first remaining word |
| `nice` | `-n VALUE`, `--adjustment=…`, `-<digits>` | the first remaining word |
| `nohup` | `--` | the first remaining word |
| `timeout` | `-s`/`--signal` and `-k`/`--kill-after` take a value; `--preserve-status --foreground -v --verbose` | the word after one DURATION word |
| `xargs` | `-I -L -n -P -s -d -E -a` take a value; `-i… -l… -e…` with optional attached values; other `-…` words | the first remaining word, or `echo` when none, plus one unknown operand for what xargs reads from its input |
| `command` | `-p` | the first remaining word; none for `-v`/`-V` |
| `exec` | `-c -l`, `-a NAME` | the first remaining word |
| `find` | — | the words between each `-exec`/`-execdir`/`-ok`/`-okdir` and the next `;` or `+`, with `{}` an unknown operand |
| `bash`, `sh`, `dash`, `zsh`, `ksh`, `fish` | an option cluster containing `c` (`-c`, `-lc`, `-xec`) | the script word, lexed and walked when it is literal |

A value-taking option skips the next word unless its value is attached (`-uroot`,
`--user=root`). `fish` scripts are read as POSIX shell, best-effort.

## Unclear commands

When the guard cannot tell what a command will run, the command is unclear, and `on_unclear`
decides: `deny` (the default) refuses the call, `allow` lets it run. An unclear command
refuses only when no command matched a rule or pattern.

| Case | Example |
|---|---|
| The script cannot be lexed | `rm 'x` |
| Substitutions or `-c` scripts nest deeper than 16 levels | 17 nested `$(` |
| `eval` with any arguments | `eval "$X"` |
| The program word is built when the command runs | `$CMD -rf x`, `` `echo rm` -rf x `` |
| A shell interpreter's `-c` script is not literal, its script operand is a process substitution, or an expansion stands among its options with more words after it (it could be `-c`) | `bash -c "$X"`, `bash <(curl -s URL)`, `bash $OPTS -c 'rm -rf x'` |
| A shell interpreter reads its script from its input — no `-c` and no script operand, only `-s` or `-`, or a lone expansion that could be `-s` — and it sits after the first stage of a pipeline or its input is redirected | `echo … \| base64 -d \| sh`, `sh <<EOF`, `… \| bash "$O"` |
| A just shebang recipe in a language other than shell | `#!/usr/bin/env python3` |
| A rule's program, subcommand and flag conditions hold, but no literal operand satisfies its operand conditions while an operand is not known | `rm -r "$d"` under a `path_is` rule |

For the last case, the arguments `xargs` reads from its input and `find -exec`'s `{}` are
operands that are not known.

## Refusal reasons

The first hit wins: commands in source order, and for each command `deny_programs` first, then
`rules` in config order; then `patterns`; then, under `on_unclear: deny`, the first unclear
command. The text after `matched` and inside backticks is collapsed to single spaces and
clipped to 200 characters, ending in `…` when clipped.

| Source | Reason |
|---|---|
| `deny_programs` | ``deny_programs lists `<entry>`; matched `<command text>` `` |
| rule | ``rules[<i>] (<name>): <reason>; matched `<command text>` ``, without ` (<name>)` when the rule has no name |
| pattern | ``patterns[<i>]: <reason>; matched `<matched text>` `` |
| unclear | ``on_unclear is deny and <what>: `<command text>` `` |
| rule that may apply | ``on_unclear is deny and rules[<i>] (<name>) may apply: `<word>` is not known until the command runs; matched `<command text>` `` |

`<command text>` is the simple command as written, wrappers included, so `sudo rm -rf x`
quotes `sudo rm -rf x`. For a direct call it is the reassembled command line.

`<what>` is one of:

| Case | `<what>` |
|---|---|
| lex error | `the guard could not read the script (<error>)` |
| `eval` | `eval runs a string the guard cannot read` |
| program built at run time | `the command name is assembled when the command runs` |
| interpreter script not literal | `a shell runs a script the guard cannot read` |
| interpreter reads its input | `a shell reads its script from its input` |
| nesting too deep | `the script nests deeper than the guard reads` |
| just shebang recipe | `the recipe runs in <interpreter>, which the guard cannot read` |

A config the guard cannot fully read refuses every command with:

```text
hook-command-guard's config is invalid, so every command is refused: <path>: <problem>
```

`<path>` names the field exactly — `rules[2].all_flags[1][0]`, `patterns[0].pattern`,
`on_unclear`, an unknown key such as `rulez`, or `config` for the block itself. The config is
invalid when it is not JSON, is not an object, holds an unknown key at any level, a wrong type,
an empty list, an empty string, a flag spelling without a leading `-`, a misplaced `*`, an
unusable path, or a regular expression that does not compile within the size limit.

The runtime adds the hook's name and the warning that an unchanged retry is refused again.

## Limits

The guard stops accidents and plain misuse, not a determined agent: an agent that writes a script file and runs it, or hands code to an interpreter (`python3 -c`, `node -e`, `perl -e`), is not stopped.

- The guard is a second gate in front of `capabilities.shell.allow`, which remains the
  boundary. Under `--unconfined` it may be the only thing between the agent and the host.
- A direct call is matched on the resolved file's basename: on this kind of host `sh` resolves
  to `dash`, and on a rustup host `cargo` resolves to `rustup`. List both names in `program`.
  The event does not carry the name the agent invoked.
- An absent recipe body is not unclear, and make's `$(shell …)` is not read.
- `bash script.sh` and `source file` run a file the guard does not read.
- Command substitutions inside a heredoc body are not read.
- Option clusters are expanded for every program, so a value attached to a short option
  (`-mfix`) reads as more flags. Long-option abbreviations (`--forc`) are not recognized.
- `git push` with no refspec while on `main` is not caught.
- A subcommand is found only as the first or second operand, so `cargo +nightly publish` and
  `git -C dir -c key=value push` do not match a `subcommand` rule.
- Brace expansion is not performed: `/{etc,usr}` is read as one literal path.
- A word holding an expansion never satisfies a flag condition, so `rm $(echo -rf) x` does not
  match a rule that needs `-r` and `-f`.
- Wrappers beyond the table above — `doas`, `stdbuf`, `ionice`, `setsid`, `flock`, `watch`,
  `coproc`, `builtin` and others — are not looked through, and the command string a `trap`
  runs later is not read.
- Aliases and functions defined earlier in the session are not known.
