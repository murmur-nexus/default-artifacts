# The ideal layout, L1–L9

R1–R7 in `requirements.md` are what MAP needs to list a repository. L1–L9 are what keeps it easy to
release from and safe to install from, on top of them. Each item gives the rule, the read-only
command that checks it, what output means pass, and the change that fixes a miss. A change marked
**you run** changes GitHub: hand it to the person verbatim and never run it yourself.

The commands use the names `requirements.md` sets (`OWNER`, `REPO`, `SCOPE`, `WORK`) and run from the
repository's root.

## The tree

```
murmur-artifacts/
├── .github/
│   └── workflows/
│       └── release.yml          from assets/release.yml (L5)
├── LICENSE                       (L8)
├── README.md                     what the repository serves and how to install it
├── tools/
│   └── <name>/                   one folder per artifact, named as the artifact (L1)
│       ├── murmur.yaml           name, version, runtime, ... (L3)
│       ├── README.md             (L3)
│       └── ...                   sources, or the built component the manifest packs
├── drivers/<name>/               same form as tools/
├── hooks/<name>/                 same form as tools/
└── skills/
    └── <name>/                   a standard Agent Skills folder (L2)
        ├── SKILL.md              front block: name, description, metadata.version
        └── references/...        only files SKILL.md names
```

Leave out the category folders the repository has no artifact for.

---

## L1 — one folder per artifact, named as the artifact

**Rule.** Every artifact has its own folder `tools/<name>/`, `drivers/<name>/`, `hooks/<name>/` or
`skills/<name>/`, and the folder's name equals the artifact's name. `assets/release.yml` finds the
folder to build from the tag's name this way.

**Check.**

```bash
for d in tools/*/ drivers/*/ hooks/*/; do [ -f "$d/murmur.yaml" ] && printf '%s %s\n' "$(basename "$d")" "$(sed -nE 's/^name:[[:space:]]*["'\'']?([^"'\'' ]+).*/\1/p' "$d/murmur.yaml")"; done
for d in skills/*/; do [ -f "$d/SKILL.md" ] && printf '%s %s\n' "$(basename "$d")" "$(awk 'NR==1{f=($0=="---");next} f&&/^---$/{exit} f&&/^name:/{sub(/^name:[[:space:]]*/,""); gsub(/["'\'']/,""); print}' "$d/SKILL.md")"; done
git ls-files '*murmur.yaml' '*SKILL.md' '*.mur.zip'
```

**Pass.** Each printed line is two equal words, and the last command lists no manifest or zip
outside those folders. A built `.mur.zip` is never committed: releases carry them.

**Fix.** Move each artifact into its folder with `git mv`, renaming the folder to the artifact's name.

## L2 — each skill is a standard Agent Skills folder

**Rule.** A skill lives at `skills/<name>/` and holds a file named exactly `SKILL.md`, whose front
block gives `name` (equal to the folder name), `description` and `metadata.version`. Beside it are
only the files the `SKILL.md` body names by their folder-relative path, and never a `murmur.yaml`:
`mur build` writes the zip's `murmur.yaml` from the front block. The same folder installs with
`mur install <scope>/<name>` from a release and with
`npx skills add <owner>/murmur-artifacts --skill <name>` from git.

```markdown
---
name: <name>
description: "<what the skill does and when to use it, at most 1024 characters>"
metadata:
  version: "<x.y.z>"
---
```

**Check.** For each skill folder:

```bash
mur build skills/<name> --output "$WORK/"
```

**Pass.** It exits 0 and writes `$WORK/<name>-<version>.mur.zip`. A broken folder is refused with
`E-BLD-009`, naming the rule it breaks: a front block that does not parse, a `name` that differs
from the folder, a `murmur.yaml` beside `SKILL.md`, a `SKILL.md` spelled any other way. This needs
a `mur` that builds `SKILL.md` folders (0.11.0 or later); an older one refuses every such folder.

**Fix.** The change `E-BLD-009` names. For the folder itself, see
`https://docs.murmur.nexus/how-to/package-skill-artifact/`.

## L3 — a tool, driver or hook folder holds its manifest and a README

**Rule.** Each folder under `tools/`, `drivers/` and `hooks/` holds its `murmur.yaml` and a
`README.md` that says what the artifact does and what it needs.

**Check.**

```bash
for d in tools/*/ drivers/*/ hooks/*/; do [ -d "$d" ] || continue; for f in murmur.yaml README.md; do [ -f "$d$f" ] || echo "missing: $d$f"; done; done
```

**Pass.** It prints nothing.

**Fix.** Write the missing file. For a `murmur.yaml`, use the skill `skill-create-manifest`.

## L4 — `main` equals the last released state

**Rule.** The default branch, `main`, holds exactly what the releases hold: every artifact folder on
`main` is at a version that has been released, unchanged since its tag. Unreleased work stays on a
branch until its release is published. `npx skills` installs from the default branch, so a skill
change on `main` reaches users before its release does.

**Check.** For each artifact folder on `main`, with its `<name>` and `<version>`:

```bash
git fetch --tags origin
git rev-parse -q --verify "refs/tags/<name>-v<version>" >/dev/null && echo tagged || echo untagged
git diff --quiet "<name>-v<version>" origin/main -- <folder> && echo same || echo changed
```

The tag form is R6's.

**Pass.** Every folder prints `tagged` and `same`.

**Fix.** Move the unreleased change from `main` to a branch, or release it (situation (d) in
`SKILL.md`). Never move a tag.

## L5 — one release workflow

**Rule.** The repository has one workflow, `.github/workflows/release.yml`, copied from
`assets/release.yml` with its placeholders filled in. On a pushed tag it builds the tagged artifact,
writes the merged index, and uploads both to a new release.

**Check.**

```bash
ls .github/workflows/
grep -nE "tags:|mur build|mur index|--merge|--draft" .github/workflows/release.yml
grep -nE '<[a-z][^>]*>' .github/workflows/release.yml | grep -vE '^[0-9]+:[[:space:]]*#'
```

**Pass.** `release.yml` exists and is the only workflow that creates releases; the first grep
finds the tag trigger, `mur build`, `mur index` with `--merge`, and `--draft`; the second finds no
unfilled `<placeholder>` outside a comment.

**Fix.** Copy `assets/release.yml` to `.github/workflows/release.yml` and fill in its placeholders.

## L6 — draft, upload, then publish

**Rule.** Each release is created as a draft, every file is uploaded to it, and only then is it
published. An immutable release can never gain or change a file after it is published, and GitHub
attests a release when it is published, so the attestation covers every file only when they are
all uploaded first.

**Check.**

```bash
grep -nE "release (create|upload|edit)" .github/workflows/release.yml
```

**Pass.** In this order: `gh release create ... --draft`, `gh release upload`, and
`gh release edit ... --draft=false`. A workflow that passes the files to `gh release create`
without `--draft` fails this item.

**Fix.** Use the steps of `assets/release.yml`.

## L7 — release immutability is on, before the first release

**Rule.** The repository setting "Enable release immutability" is on. MAP labels a release
immutable only when GitHub reports it immutable and it has a release attestation, and a release
published before the setting was on has none. So the setting goes on before the first tag.

**Check.**

```bash
gh api "repos/$REPO/immutable-releases"
```

**Pass.** `{"enabled":true,...}`. `{"enabled":false,"enforced_by_owner":false}` or `HTTP 404` means
it is off: `missing`. `HTTP 403` means the account lacks admin rights on the repository:
`cannot check — needs admin`. For a release already made, check
`gh release view <tag> --repo "$REPO" --json isImmutable` (`true`) and
`gh release verify <tag> --repo "$REPO"`, which passes only for a release with an attestation.

**Fix.** **You run** `gh api -X PUT "repos/$REPO/immutable-releases"`, which answers with no output
on success; or turn it on under Settings → General → Releases → "Enable release immutability".
Releases published before it was on stay mutable: their versions are listed as mutable, and only
new versions become immutable.

## L8 — a licence

**Rule.** The repository has a licence file GitHub recognises, so installers know the terms.

**Check.**

```bash
gh repo view "$REPO" --json licenseInfo
git ls-files | grep -iE '^(LICENSE|LICENCE|COPYING)'
```

**Pass.** `licenseInfo` is not `null`, and a `LICENSE` file is tracked.

**Fix.** Ask the person which licence they want; never choose one for them. Write its text to
`LICENSE`, commit it, and **you run** `git push`.

## L9 — no secrets in the repository

**Rule.** No key, token or private key is tracked, now or in history, and no `.env` file is
committed. Report each finding by file and line, never the value.

**Check.** With `gitleaks` installed:

```bash
gitleaks detect --source . --redact --no-banner
```

Without it, these print only the location of a match:

```bash
KEYS='AKIA[0-9A-Z]{16}|gh[pousr]_[A-Za-z0-9]{36}|github_pat_[A-Za-z0-9_]{40,}|sk-[A-Za-z0-9_-]{20,}|xox[abprs]-[A-Za-z0-9-]{10,}|-----BEGIN [A-Z ]*PRIVATE KEY-----'
git grep -nIE "$KEYS" | cut -d: -f1,2
git rev-list --all | xargs -r git grep -nIE "$KEYS" | cut -d: -f1-3 | sort -u
git ls-files | grep -E '(^|/)\.env($|\.)'
```

**Pass.** `gitleaks` reports `no leaks found`, or each of the other commands prints nothing.

**Fix.** Remove the file or line, add it to `.gitignore`, and tell the person to revoke the secret,
since one in history stays readable until it is revoked. Rewriting published history is the
person's decision.

---

## An optional extra: a provenance bundle per file

`mur install` also looks for `<file>.sigstore.json` beside each `.mur.zip` on its release, a bundle
written by `actions/attest-build-provenance`, and installs a file without one with the warning
`W-REG-005`. MAP does not require it. To add it, give the workflow
`permissions: id-token: write, attestations: write`, run `actions/attest-build-provenance` on
`release/*.mur.zip` before the upload, and copy each bundle to `release/<file>.sigstore.json`.
