#!/usr/bin/env python3
"""The repository's one reader of a skill folder and its SKILL.md front block.

A skill is a folder `skills/<name>/` that follows the Agent Skills standard
(https://agentskills.io/specification): a root file named exactly `SKILL.md`
whose front block holds the skill's `name`, `description` and, for `mur build`,
`metadata.version`. That front block is the only place a skill's name and
version are written. `mur build <folder>` packs the folder and generates the
zip's murmur.yaml from the front block; `npx skills` installs the folder as is.

Used three ways, so the rules live in one place:

    check-skill-folders.sh   runs `skill_folder.py check <folder>...`
    list-artifacts.sh        runs `skill_folder.py rows <folder>...`
    check-version-drift.sh   imports parse_front_block() to read a SKILL.md at a git ref

The front block is YAML, read here by a small stdlib-only reader rather than
PyYAML, because CI runs these scripts on a stock runner. It reads what a skill
front block needs and refuses, by key, anything else:

    accepted   top-level `key: value` lines at column 0; one level of two-space
               indented `key: value` lines under `metadata:`; blank and `#`
               comment lines; values that are plain, 'single-quoted' (with ''
               for a quote) or "double-quoted" (with \\" \\\\ \\n \\t \\uXXXX),
               each on one line
    refused    a block scalar (`>` or `|`), a value spanning lines, a flow
               mapping or sequence, a block sequence, an anchor, alias or tag,
               a duplicate key, a tab in indentation, and a plain value holding
               `: ` or ` #`, which YAML reads as a mapping or a comment

Keys the rules do not use (`license`, `allowed-tools`, other `metadata.*`) are
read and ignored.

CLI:
    skill_folder.py check <folder>...   exit 0 and print `OK: <n> skill folder(s) ...`,
                                        or exit 1 with `error: <folder>: ...` lines on stderr
    skill_folder.py rows <folder>...    exit 0 and print `<folder>\\t<name>\\t<version>` per
                                        folder, or exit 1 with the same errors and no stdout
    Exit 2: usage error, or a named path that is not a directory.
"""

import os
import re
import sys

NAME_MAX = 64
DESCRIPTION_MAX = 1024
COMPATIBILITY_MAX = 500

NAME_CHARS = re.compile(r"^[a-z0-9-]+$")
# semver.org's pattern, the one `mur build` holds metadata.version to.
SEMVER = re.compile(
    r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
    r"(?:-((?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)(?:\.(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*))*))?"
    r"(?:\+([0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*))?$"
)
KEY_LINE = re.compile(r"^([A-Za-z0-9_][A-Za-z0-9_.-]*):(?:( +)(.*))?$")
METADATA_LINE = re.compile(r"^  ([A-Za-z0-9_][A-Za-z0-9_.-]*):(?:( +)(.*))?$")
# Plain scalars YAML would read as something other than a string.
NON_STRING_PLAIN = re.compile(
    r"^(?:~|null|Null|NULL|true|True|TRUE|false|False|FALSE"
    r"|[-+]?(?:0|[1-9][0-9]*)|0o[0-7]+|0x[0-9a-fA-F]+"
    r"|[-+]?(?:\.[0-9]+|[0-9]+(?:\.[0-9]*)?)(?:[eE][-+]?[0-9]+)?"
    r"|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN))$"
)
SUPPORTED = (
    "write it on one line, plain, 'single-quoted' or \"double-quoted\" "
    "(quote any value holding ': ' or ' #')"
)
# The keys the rules read as strings.
STRING_KEYS = ("name", "description", "compatibility", "metadata.version")
ESCAPES = {'"': '"', "\\": "\\", "n": "\n", "t": "\t", "/": "/"}


class _Refusal(Exception):
    pass


class Refused:
    """The value of a key the reader refused; its error is already in the list."""

    def __repr__(self):
        return "<refused>"


REFUSED = Refused()


def _scalar(key, raw):
    """The string value of one line's `raw` value text, or raise _Refusal."""
    if raw == "":
        return None
    first = raw[0]
    if first in ">|":
        raise _Refusal(f"{key} is a block scalar ('{first}'), which this reader does not read; {SUPPORTED}")
    if first in "{[":
        raise _Refusal(f"{key} is a flow collection ('{first}...'), which this reader does not read; {SUPPORTED}")
    if first in "&*!%@`":
        raise _Refusal(f"{key} starts with '{first}', a YAML anchor, alias, tag or reserved indicator; {SUPPORTED}")
    if raw == "-" or raw.startswith("- "):
        raise _Refusal(f"{key} is a YAML sequence, which this reader does not read; {SUPPORTED}")
    if first == '"':
        out, i = [], 1
        while i < len(raw):
            c = raw[i]
            if c == "\\":
                if i + 1 >= len(raw):
                    break
                e = raw[i + 1]
                if e in ESCAPES:
                    out.append(ESCAPES[e])
                    i += 2
                    continue
                if e == "u" and re.match(r"^[0-9a-fA-F]{4}$", raw[i + 2:i + 6]):
                    out.append(chr(int(raw[i + 2:i + 6], 16)))
                    i += 6
                    continue
                raise _Refusal(f"{key} uses the escape '\\{e}'; a double-quoted value may use \\\" \\\\ \\n \\t and \\uXXXX")
            if c == '"':
                _trailing(key, raw[i + 1:])
                return "".join(out)
            out.append(c)
            i += 1
        raise _Refusal(f"{key} opens a double-quoted value that does not close on its line; a multi-line value is not read; {SUPPORTED}")
    if first == "'":
        out, i = [], 1
        while i < len(raw):
            c = raw[i]
            if c == "'":
                if raw[i + 1:i + 2] == "'":
                    out.append("'")
                    i += 2
                    continue
                _trailing(key, raw[i + 1:])
                return "".join(out)
            out.append(c)
            i += 1
        raise _Refusal(f"{key} opens a single-quoted value that does not close on its line; a multi-line value is not read; {SUPPORTED}")
    value = raw.rstrip(" ")
    if ": " in value or value.endswith(":"):
        raise _Refusal(f"{key} is a plain value holding ': ', which YAML reads as a mapping; quote it with \"...\"")
    if " #" in value:
        raise _Refusal(f"{key} is a plain value holding ' #', which YAML reads as a comment; quote it with \"...\"")
    return value


def _typed(key, raw):
    """_scalar(key, raw), refusing a plain value YAML reads as a non-string when the
    rules read `key` as a string."""
    value = _scalar(key, raw)
    if key in STRING_KEYS and raw[0] not in "'\"" and NON_STRING_PLAIN.match(value):
        raise _Refusal(f"{key} '{value}' is read by YAML as a number, boolean or null; quote it")
    return value


def _trailing(key, rest):
    rest = rest.strip(" ")
    if rest and not rest.startswith("#"):
        raise _Refusal(f"{key} has text after its closing quote: {rest!r}; {SUPPORTED}")


def split_front_block(text):
    """(front block lines, body) for a SKILL.md's text, or None when the text does not
    open with a `---` line closed by a later `---` line."""
    lines = text.split("\n")
    if not lines or lines[0] != "---":
        return None
    for i in range(1, len(lines)):
        if lines[i] == "---":
            return lines[1:i], "\n".join(lines[i + 1:])
    return None


def parse_front_block(text):
    """Read a SKILL.md's front block.

    Returns (fields, body, errors). `fields` maps each top-level key to its string
    value (None for an empty value, REFUSED for one the reader refused), except
    `metadata`, which maps to a dict of its own keys read the same way, or to a
    string, None or REFUSED when it is not a mapping. `body` is the text
    after the closing `---`. `errors` lists every refusal, each a sentence naming
    the key or the line; when it is non-empty, `fields` holds what was read before
    and around the refusals and is not to be trusted as complete.
    """
    if "\r" in text:
        return {}, text, ["SKILL.md has CR line endings; save it with LF line endings"]
    if text.startswith("\ufeff"):
        return {}, text, ["SKILL.md starts with a byte-order mark; save it as UTF-8 without one"]
    if not text.startswith("---\n") and text != "---":
        return {}, text, ["SKILL.md does not open with a front block: its first line must be exactly '---'"]
    split = split_front_block(text)
    if split is None:
        return {}, text, ["SKILL.md's front block is not closed: add a line that is exactly '---' after its last key"]
    block, body = split

    fields, errors = {}, []
    current = None  # the top-level key the indented lines belong to
    for n, line in enumerate(block, start=2):
        if not line.strip() or line.lstrip(" ").startswith("#"):
            continue
        if line.startswith("\t") or re.match(r"^ *\t", line):
            errors.append(f"SKILL.md line {n} is indented with a tab; indent with spaces")
            continue
        if line.startswith(" "):
            if current is None:
                errors.append(f"SKILL.md line {n} is indented but follows no key")
                continue
            if current != "metadata" or not (fields["metadata"] is None or isinstance(fields["metadata"], dict)):
                errors.append(f"{current} continues on line {n}; a multi-line value is not read; {SUPPORTED}")
                current = None
                continue
            if fields["metadata"] is None:
                fields["metadata"] = {}
            m = METADATA_LINE.match(line)
            if not m:
                errors.append(
                    f"SKILL.md line {n}: under metadata write two-space indented 'key: value' lines, one level deep"
                )
                continue
            key, value = m.group(1), m.group(3) or ""
            if key in fields["metadata"]:
                errors.append(f"metadata.{key} is set twice; keep one")
                continue
            if value == "":
                # The opening line of a nested mapping, or a null value.
                errors.append(f"metadata.{key} has no value; a metadata entry is a 'key: value' line, one level deep")
                fields["metadata"][key] = None
                continue
            try:
                fields["metadata"][key] = _typed(f"metadata.{key}", value)
            except _Refusal as refusal:
                errors.append(str(refusal))
                fields["metadata"][key] = REFUSED
            continue
        m = KEY_LINE.match(line)
        if not m:
            errors.append(f"SKILL.md line {n} is not a 'key: value' line: {line!r}")
            current = None
            continue
        key, value = m.group(1), m.group(3) or ""
        if key in fields:
            errors.append(f"{key} is set twice in the front block; keep one")
            current = None
            continue
        current = key
        if value == "":
            fields[key] = None
            continue
        try:
            fields[key] = _typed(key, value)
        except _Refusal as refusal:
            errors.append(str(refusal))
            fields[key] = REFUSED
            current = None
    return fields, body, errors


def check_front_block(fields, folder_name):
    """(name, version, errors) for parsed front-block `fields` in a folder named
    `folder_name`. name and version are None when absent or invalid."""
    errors = []
    name = fields.get("name")
    if name is REFUSED:
        name = None
    elif name is None or name == "":
        errors.append(f"SKILL.md front block has no name; add 'name: {folder_name}'")
        name = None
    else:
        bad = []
        if len(name) > NAME_MAX:
            bad.append(f"is {len(name)} characters; a skill name is at most {NAME_MAX}")
        if not NAME_CHARS.match(name):
            bad.append("holds characters other than lowercase letters, digits and '-'")
        if name.startswith("-") or name.endswith("-"):
            bad.append("starts or ends with '-'")
        if "--" in name:
            bad.append("holds '--'")
        for b in bad:
            errors.append(f"name '{name}' {b}")
        if name != folder_name:
            errors.append(f"name '{name}' differs from the folder name '{folder_name}'; set 'name: {folder_name}' or rename the folder")
        if name.startswith("murmur-"):
            errors.append(
                f"name '{name}' starts with 'murmur-'; drop the prefix — a default artifact installs as 'murmur/{name[len('murmur-'):]}'"
            )
        if bad:
            name = None

    description = fields.get("description")
    if description is REFUSED:
        pass
    elif description is None or description == "":
        errors.append("SKILL.md front block has no description; add 'description: \"<what the skill does and when to use it>\"'")
    elif len(description) > DESCRIPTION_MAX:
        errors.append(f"description is {len(description)} characters; a description is at most {DESCRIPTION_MAX}")

    compatibility = fields.get("compatibility")
    if isinstance(compatibility, str) and len(compatibility) > COMPATIBILITY_MAX:
        errors.append(f"compatibility is {len(compatibility)} characters; it is at most {COMPATIBILITY_MAX}")

    version = None
    if "metadata" not in fields:
        errors.append("SKILL.md front block has no metadata; add 'metadata:' with an indented 'version: \"<x.y.z>\"' line")
    elif fields["metadata"] is REFUSED:
        pass
    elif not isinstance(fields["metadata"], dict):
        errors.append("metadata is not a mapping; write 'metadata:' with an indented 'version: \"<x.y.z>\"' line")
    elif "version" not in fields["metadata"]:
        errors.append("metadata holds no version; add an indented 'version: \"<x.y.z>\"' line under metadata")
    elif fields["metadata"]["version"] is None or fields["metadata"]["version"] is REFUSED:
        pass  # refused by the reader already: an entry with no value
    elif fields["metadata"]["version"] == "":
        errors.append("metadata.version is empty; write MAJOR.MINOR.PATCH, e.g. version: \"1.2.0\"")
    else:
        candidate = fields["metadata"]["version"]
        if SEMVER.match(candidate):
            version = candidate
        else:
            errors.append(f"metadata.version '{candidate}' is not a semver version; write MAJOR.MINOR.PATCH, e.g. version: \"1.2.0\"")
    return name, version, errors


def check_skill_md_text(text, folder_name):
    """(name, version, body, errors) for a SKILL.md's text in a folder named `folder_name`."""
    fields, body, errors = parse_front_block(text)
    if errors and not fields:
        return None, None, body, errors
    name, version, more = check_front_block(fields, folder_name)
    return name, version, body, errors + more


def _rel(root, path):
    return os.path.relpath(path, root).replace(os.sep, "/")


def check_folder(path):
    """(name, version, errors) for the skill folder at `path`. Every rule is checked,
    and each broken rule adds one sentence to `errors`; name and version are None
    unless the front block yields them."""
    errors = []
    folder_name = os.path.basename(os.path.normpath(os.path.abspath(path)))
    entries = os.listdir(path)

    # Exact directory entries, never a stat: a case-insensitive filesystem would
    # answer for skill.md when asked for SKILL.md.
    misspelled = sorted(e for e in entries if e.lower() == "skill.md" and e != "SKILL.md")
    for e in misspelled:
        if "SKILL.md" in entries:
            errors.append(f"{e}: the skill file is named SKILL.md, exactly; delete {e}, the folder already holds SKILL.md")
        else:
            errors.append(f"{e}: the skill file is named SKILL.md, exactly; rename it: git mv {e} SKILL.md")
    # `mur build` refuses a root murmur.yaml in any casing, and a root murmur-build.json,
    # which it writes itself.
    manifests = sorted(e for e in entries if e.lower() == "murmur.yaml")
    for e in manifests:
        errors.append(f"{e}: a skill folder holds no murmur.yaml; its name, description and version go in the SKILL.md front block")
    reserved = ["murmur-build.json"] if "murmur-build.json" in entries else []
    if reserved:
        errors.append("murmur-build.json: mur build writes this file itself; delete it from the skill folder")

    symlinks = []
    for dirpath, dirnames, filenames in os.walk(path):
        for e in dirnames + filenames:
            if os.path.islink(os.path.join(dirpath, e)):
                symlinks.append(_rel(path, os.path.join(dirpath, e)))
    for s in sorted(symlinks):
        errors.append(f"{s} is a symlink; a skill folder holds no symlinks — replace it with the file itself")

    skill_md = os.path.join(path, "SKILL.md")
    name = version = None
    if "SKILL.md" not in entries:
        if not misspelled:
            errors.append("no SKILL.md at the folder root; a skill folder holds a file named SKILL.md, exactly")
        return name, version, errors
    if os.path.islink(skill_md) or not os.path.isfile(skill_md):
        if not os.path.islink(skill_md):
            errors.append("SKILL.md is not a regular file")
        return name, version, errors

    with open(skill_md, "rb") as fh:
        raw = fh.read()
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError:
        errors.append("SKILL.md is not UTF-8")
        return name, version, errors

    name, version, body, more = check_skill_md_text(text, folder_name)
    errors.extend(more)

    already = {*manifests, *reserved, *misspelled}
    unreferenced = []
    for dirpath, dirnames, filenames in os.walk(path):
        dirnames[:] = sorted(d for d in dirnames if not os.path.islink(os.path.join(dirpath, d)))
        for f in sorted(filenames):
            full = os.path.join(dirpath, f)
            rel = _rel(path, full)
            if rel == "SKILL.md" or rel in already or os.path.islink(full) or not os.path.isfile(full):
                continue
            if rel not in body:
                unreferenced.append(rel)
    for rel in unreferenced:
        errors.append(
            f"{rel} is not named in the SKILL.md body; name it there by its folder-relative path '{rel}', or delete it"
        )
    return name, version, errors


def main(argv):
    if len(argv) < 3 or argv[1] not in ("check", "rows") or any(a.startswith("-") for a in argv[2:]):
        print("usage: skill_folder.py check|rows <folder>...", file=sys.stderr)
        return 2
    mode, folders = argv[1], argv[2:]
    for f in folders:
        if not os.path.isdir(f):
            print(f"error: {f}: not a directory", file=sys.stderr)
            return 2
    rows, failed = [], False
    for f in folders:
        label = f.rstrip("/") or f
        name, version, errors = check_folder(f)
        for e in errors:
            print(f"error: {label}: {e}", file=sys.stderr)
        if errors:
            failed = True
        else:
            rows.append(f"{label}\t{name}\t{version}")
    if failed:
        return 1
    if mode == "rows":
        for r in rows:
            print(r)
    else:
        print(f"OK: {len(folders)} skill folder(s) follow the Agent Skills standard")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
