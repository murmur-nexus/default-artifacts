# MAP's minimum requirements, R1–R7

MAP lists nothing from a repository until all seven pass. Each requirement below gives the rule,
the read-only commands that check it, what in their output means pass, and the change that fixes a
miss. A change marked **you run** changes GitHub: hand it to the person verbatim and never run it
yourself.

The commands use these names. Set them once:

```bash
OWNER=<the GitHub login that owns the scope>   # a user or an organization
REPO="$OWNER/murmur-artifacts"
SCOPE=$(printf '%s' "$OWNER" | tr '[:upper:]' '[:lower:]')
WORK=$(mktemp -d)                              # scratch space for downloads
```

When the repository does not exist yet, R1–R7 are all `missing`, and the change for each is the
setup of situation (a) in `SKILL.md`.

---

## R1 — a public GitHub repository owned by the scope owner

**Rule.** The repository is public, and its owner's login, lowercased, is the scope its artifacts
install under. The scope is neither `murmur` nor `local`: both are reserved, and `mur` refuses them
for any other source (`E-REG-007`).

**Check.**

```bash
gh repo view "$REPO" --json visibility,owner,name,repositoryTopics,licenseInfo,defaultBranchRef
```

**Pass.** `"visibility":"PUBLIC"`, and `owner.login` lowercased equals `$SCOPE`, which is not
`murmur` or `local`. A reply of `Could not resolve to a Repository` means the repository does not
exist: R1 is `missing`.

**Fix.** The repository does not exist: **you run** `gh repo create "$REPO" --public --source . --push`
from the prepared local folder. It exists but is private: **you run**
`gh repo edit "$REPO" --visibility public --accept-visibility-change-consequences`. The owner is
wrong: the repository has to live under the account that owns the scope; create it there.

## R2 — named `murmur-artifacts`

**Rule.** The repository's name is exactly `murmur-artifacts`. MAP's crawler accepts no other name.
The one exception is the reserved `murmur` scope, which `murmur-nexus/default-artifacts` serves;
no publisher's repository is that exception.

**Check.** The same `gh repo view` as R1.

**Pass.** `"name":"murmur-artifacts"`.

**Fix.** A home repository under another name: **you run** `gh repo rename murmur-artifacts --repo <owner>/<old-name>`.
A project repository that builds artifacts on the side keeps its name; the artifacts get a separate
home repository (situation (b) in `SKILL.md`).

## R3 — carries the `murmur-artifacts` topic

**Rule.** The repository has the topic `murmur-artifacts`. The nightly crawl finds home repositories
through this topic.

**Check.**

```bash
gh repo view "$REPO" --json repositoryTopics --jq '[.repositoryTopics[]?.name]'
```

**Pass.** The list holds `"murmur-artifacts"`. `[]` means there is no topic: `repositoryTopics` is
`null` when a repository has none.

**Fix.** **You run** `gh repo edit "$REPO" --add-topic murmur-artifacts`.

## R4 — the index on the latest release

**Rule.** The **latest** release carries an asset named `artifacts-index.json`, written by
`mur index`, so it answers at
`https://github.com/<owner>/murmur-artifacts/releases/latest/download/artifacts-index.json`.
`mur install` and MAP read the index of the latest release only, so every release carries an index
that also lists every earlier version (`mur index --merge`, as `assets/release.yml` does).

**Check.**

```bash
gh release view --repo "$REPO" --json tagName,isDraft,isImmutable,assets --jq '{tagName, assets: [.assets[].name]}'
gh release download --repo "$REPO" --pattern artifacts-index.json --dir "$WORK"
mur search "" --index "$WORK/artifacts-index.json" --limit 1000
python3 -I -c 'import json,sys; i=json.load(open(sys.argv[1])); print(i["schema_version"], i["scope"]); [print(a["name"], v["version"], v["release"], f["file"]) for a in i["artifacts"] for v in a["versions"] for f in v["assets"]]' "$WORK/artifacts-index.json"
```

With no tag, `gh release view` and `gh release download` read the latest release. `mur search ""`
matches every artifact, and `--index` also takes the URL above.

**Pass.**

- `gh release view` names a tag, and its assets hold `artifacts-index.json`;
- `mur search` exits 0 and prints a table with one `<scope>/<name>` row per artifact;
- the Python line prints `2 <scope>` first: `schema_version` is 2 and `scope` equals `$SCOPE`;
- every `<release> <file>` pair it prints is an asset of that release (checked under R5).

`release not found` from `gh release view` means the repository has no published release: R4 is
`missing` (and so are R5 and R6), and the change is the first release. `no assets match the file
pattern` means the latest release carries no index: R4 is `missing`.

**Fix.** Release through `assets/release.yml`, copied to `.github/workflows/release.yml` (layout L5),
and **you run** the tag push R6 names. A latest release with no index is never edited: the next
release, made by the workflow, carries the index.

## R5 — one release per artifact version, holding its file

**Rule.** Every version the index lists has a published GitHub release, the one the index names as
its `release`, with that version's `<name>-<version>.mur.zip` attached. A release is never left a
draft.

**Check.** For each `<name> <version> <release> <file>` line the R4 Python line printed:

```bash
gh release view <release> --repo "$REPO" --json isDraft,assets --jq '{isDraft, assets: [.assets[].name]}'
```

**Pass.** `"isDraft":false`, and the assets hold `<file>`, which is `<name>-<version>.mur.zip`.

**Fix.** A version with no release or no file: release that version again through the workflow, at
a new version if the old tag already exists, because a released file is never replaced. A draft:
**you run** `gh release edit <release> --repo "$REPO" --draft=false` once every file is uploaded.

## R6 — the same version in file name, manifest and tag

**Rule.** One artifact version has one semantic version, written identically in three places:

| Place | Form | Example |
|---|---|---|
| The file name | `<name>-<version>.mur.zip` | `tool-weather-0.7.0.mur.zip` |
| The `murmur.yaml` packed inside the zip | `name: <name>` and `version: <version>` | `name: tool-weather`, `version: 0.7.0` |
| The release tag | `<name>-v<version>` | `tool-weather-v0.7.0` |

**This is the tag convention.** One release per artifact version, tagged `<name>-v<version>`. In a
repository with more than one artifact, only a tag that also names the artifact can carry the same
version as the file and the manifest. `assets/release.yml` parses this form and refuses a tag that
does not match it.

**Check.** For each version the index lists:

```bash
gh release download <release> --repo "$REPO" --pattern '<name>-<version>.mur.zip' --dir "$WORK"
unzip -p "$WORK/<name>-<version>.mur.zip" murmur.yaml | grep -E '^(name|version):'
```

Without `unzip`, extract with `python3 -m zipfile -e "$WORK/<name>-<version>.mur.zip" "$WORK/x"` and
read `$WORK/x/murmur.yaml`.

**Pass.** The file name is `<name>-<version>.mur.zip`, the packed `murmur.yaml` prints
`name: <name>` and `version: <version>` (quoted or not: `mur build` writes `version: '0.1.0'`), and `<release>`
is `<name>-v<version>`. A packed version with a `v` prefix or build metadata (`+...`) the tag does
not carry is a mismatch.

**Fix.** Release the version again under the right tag: a new version in the artifact's own
manifest (situation (d) in `SKILL.md`), and **you run** `git tag <name>-v<version> && git push origin <name>-v<version>`.
An existing release or tag is never moved or re-uploaded.

## R7 — artifact names unique within the scope

**Rule.** No two artifacts in the repository declare the same name, and the index lists each name
once. A name is `name:` in a `murmur.yaml`, or `name` in a `SKILL.md` front block.

**Check.** In the repository's working tree:

```bash
{ git ls-files '*murmur.yaml' | xargs -r grep -h '^name:' ;
  git ls-files '*SKILL.md' | xargs -r awk 'FNR==1{f=($0=="---")} f&&FNR>1&&/^---$/{f=0} f&&/^name:/' ; } \
  | sed -E 's/^name:[[:space:]]*//; s/^["'\'']//; s/["'\'']$//' | sort | uniq -d
python3 -I -c 'import json,sys,collections; n=[a["name"] for a in json.load(open(sys.argv[1]))["artifacts"]]; print([k for k,c in collections.Counter(n).items() if c>1])' "$WORK/artifacts-index.json"
```

**Pass.** The first command prints nothing, and the second prints `[]`. With no index yet, only the
first applies; with no artifact folder either, there is nothing to check.

**Fix.** Rename one of the two artifacts, in its folder name and its manifest.
