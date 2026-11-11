---
name: skill-publish-artifact
description: "Make a GitHub repository ready for MAP (map.murmur.nexus) to list its Murmur .mur.zip artifacts. Works out the situation — no publishing repo yet, a project repo that builds artifacts on the side, an existing murmur-artifacts repo, or a new version of a listed artifact — then checks MAP's minimum requirements (a public repo named murmur-artifacts with its topic, artifacts-index.json on the latest release, one release per version, the same version in file, manifest and tag, unique names) and the ideal layout with gh, git and mur, and writes a readiness report before changing anything. Hands the person every command that changes GitHub instead of running it. Use when publishing Murmur artifacts, setting up or auditing a murmur-artifacts repo, releasing a new version of an artifact, or finding out why MAP does not list one."
compatibility: Requires mur and gh on the host, and a GitHub account that owns the publishing scope
metadata:
  version: "0.1.0"
---
# Publish Murmur artifacts so MAP lists them

## What this is for

MAP, at `https://map.murmur.nexus`, is the public catalogue of Murmur artifacts: every night it
finds the GitHub repositories named `murmur-artifacts` that carry the `murmur-artifacts` topic, reads
the artifact index on each one's latest release, and lists the artifacts whose releases pass its
checks. Each listed artifact gets a page at `https://map.murmur.nexus/<scope>/<name>`, where the
scope is the repository owner's login, lowercased, and anyone can install it with
`mur install <scope>/<name>`.

This skill is about the **repository**, not one artifact: it makes the repository that publishes a
person's artifacts meet MAP's requirements, and releases new versions from it. Building or
packaging an artifact itself is elsewhere:

| For | See |
|---|---|
| An artifact's `murmur.yaml` | the skill `skill-create-manifest`: `npx skills add murmur-nexus/default-artifacts --skill skill-create-manifest` |
| A skill artifact | `https://docs.murmur.nexus/how-to/package-skill-artifact/` |
| A WASM tool | `https://docs.murmur.nexus/language-guides/python/` |
| The build, index and upload commands | `https://docs.murmur.nexus/how-to/publish-artifact-source/` |
| The guide to give the person | `https://docs.murmur.nexus/how-to/publish-an-artifact/` |

The supporting files hold the detail:

| File | Holds |
|---|---|
| `references/requirements.md` | R1–R7, MAP's minimum requirements, each with its command, pass condition and fix |
| `references/layout.md` | L1–L9, the ideal layout, each in the same form, and the folder tree |
| `assets/release.yml` | the release workflow to copy into the repository: build, index, draft, upload, publish |

## Ground rules for you, the agent

- **Your first output is the readiness report.** Gather the facts, check, and write the report
  before you change or propose to run anything.
- **Change nothing until the person agrees,** item by item. Make exactly the changes they agree to.
- **Never run a command that changes GitHub.** Give the person the exact command instead, and let
  them run it. These are theirs:
  - `gh repo create`, `gh repo edit` (topics, visibility, settings), `gh repo rename`;
  - `git push`, in any form;
  - `git tag` and `git push --tags`, or a push of one tag;
  - `gh api -X PUT repos/<owner>/murmur-artifacts/immutable-releases`, and any other `gh api` call
    with `-X POST`, `PUT`, `PATCH` or `DELETE`, or with `-f`/`-F` fields (which make it a `POST`);
  - `gh release create`, `upload`, `edit`, `delete`, or any other `gh release` command that writes.
- **Read-only commands are fine without asking:** `gh auth status`, `gh repo view`,
  `gh release view`, `gh release list`, `gh release download`, `gh release verify`, a `gh api` GET,
  `git status`, `git log`, `git diff`, `git fetch`, `git ls-remote`, `gh repo clone`, `mur build`
  into a scratch folder, `mur index` into a scratch folder, `mur search`.
- **Never print a secret's value.** When you find one, report its file and line only.
- **Never replace or re-upload a released file.** A change is always a new version.

## Find your situation

Run these in the folder you were started in:

```bash
git rev-parse --show-toplevel          # "not a git repository" means no repo here
git remote get-url origin              # the repository's GitHub owner and name
ls -a
find . -name .git -prune -o \( -name murmur.yaml -o -name SKILL.md -o -name '*.mur.zip' \) -print
grep -rIl --exclude-dir=.git -e 'mur build' . | head -20
```

The last two find artifact manifests, skill folders, built artifacts, and the scripts or workflows
that build them. Then decide:

| Situation | You see | What to do |
|---|---|---|
| **(a)** No publishing repo yet | No git repository, or an empty folder, or loose artifact sources or `.mur.zip` files | Check whether `<owner>/murmur-artifacts` already exists (see below). If it does, `gh repo clone` it and continue as (c). If not, plan a new `murmur-artifacts` repository |
| **(b)** A project repo that builds artifacts on the side | A git repository not named `murmur-artifacts` that holds an artifact folder or runs `mur build` | Leave the project repository as it is, and plan a separate `murmur-artifacts` home repository that builds from it |
| **(c)** A `murmur-artifacts` repo | `git remote get-url origin` ends in `/murmur-artifacts` or `/murmur-artifacts.git` | Audit it against R1–R7 and L1–L9 |
| **(d)** A new version of a listed artifact | The person asks to release a new version of an artifact that the latest index already lists | (c), plus the version change. Run the (c) audit too, and report both |

When the folder fits none of these, say what you found and ask the person which repository they
mean to publish from.

## Gather the facts (read-only)

**The account and the scope.**

```bash
gh auth status
gh api user --jq .login
gh api user/orgs --jq '.[].login'
```

The scope owner is the account the artifacts will install under: the logged-in user, or one of the
organizations it belongs to. Ask the person when that is not plain. Set `OWNER` to that login and
`SCOPE` to it lowercased. `murmur` and `local` are reserved scopes: if `SCOPE` is either, stop and
tell the person to publish from another account.

**The home repository.**

```bash
gh repo view "$OWNER/murmur-artifacts" --json visibility,owner,name,repositoryTopics,licenseInfo,defaultBranchRef
```

`Could not resolve to a Repository` means it does not exist yet.

**Its latest release and index.** Run the R4 commands in `references/requirements.md`. `release not
found` means it has no published release yet.

**The local artifacts.** For each folder holding a `murmur.yaml`, read its `name:` and `version:`.
For each folder holding a `SKILL.md`, read the front block's `name` and `metadata.version`:

```bash
grep -E '^(name|version):' <folder>/murmur.yaml
sed -n '1,/^---$/{/^name:/p;/^  version:/p}' <folder>/SKILL.md | head -2
```

Note each artifact's folder, name, version, and whether the latest index lists that version.

## The minimum requirements MAP checks

MAP lists nothing from the repository until R1–R7 all pass. `references/requirements.md` gives each
one's command, pass condition and fix.

- **R1** A public GitHub repository owned by the scope owner; the scope is not `murmur` or `local`.
- **R2** Named `murmur-artifacts`.
- **R3** Carries the `murmur-artifacts` topic.
- **R4** The `artifacts-index.json` that `mur index` writes is an asset of the latest release, and
  `mur search ""` reads it.
- **R5** One published release per artifact version, holding that version's
  `<name>-<version>.mur.zip`.
- **R6** The same semantic version in the file name, the packed `murmur.yaml` and the release tag.
  R6 defines the tag form.
- **R7** Artifact names unique within the scope.

## The ideal layout

On top of R1–R7. `references/layout.md` gives the tree and each item's command, pass condition and
fix.

- **L1** One folder per artifact, `tools/`, `drivers/`, `hooks/` or `skills/<name>/`, named as the
  artifact.
- **L2** Each skill is a standard Agent Skills folder: `SKILL.md` with a front block, never a
  `murmur.yaml`.
- **L3** Each tool, driver or hook folder holds its `murmur.yaml` and a `README.md`.
- **L4** `main` equals the last released state; unreleased work stays on a branch.
- **L5** One workflow, `.github/workflows/release.yml` from `assets/release.yml`.
- **L6** Each release is a draft until every file is uploaded, then published.
- **L7** Release immutability is on, turned on before the first release.
- **L8** A licence.
- **L9** No secrets in the repository.

## Write the readiness report

Check every R and L item, then write the report in exactly this form, as your first output:

```markdown
# Readiness report: <owner>/murmur-artifacts

Situation: (c) — <one-line reason>

**Minimum requirements.** MAP lists nothing until all seven are in place.

| ID | Requirement | State | Evidence | Change |
|---|---|---|---|---|
| R1 | Public repo owned by the scope owner | in place | `"visibility":"PUBLIC"`, owner `ana` | — |
| R3 | Carries the `murmur-artifacts` topic | missing | `repositoryTopics` is `null` | 2 |
| ... | | | | |

**Ideal layout.**

| ID | Item | State | Evidence | Change |
|---|---|---|---|---|
| L7 | Release immutability on | missing | `{"enabled":false,...}` | 3 |
| ... | | | | |

**Proposed changes.**

1. Copy `assets/release.yml` to `.github/workflows/release.yml`. — I make this once you agree.
2. `gh repo edit ana/murmur-artifacts --add-topic murmur-artifacts` — You run this.
3. `gh api -X PUT repos/ana/murmur-artifacts/immutable-releases` — You run this.

Nothing has been changed yet. Which of these numbers do you agree to?
```

- Name the folder in the heading when there is no repository yet.
- Give one row per ID, R1–R7 and L1–L9, none left out.
- **State** is one of `in place`, `missing`, or `cannot check — <why>`.
- **Evidence** is the output that decided it, trimmed, never a secret's value.
- **Change** is the number of the proposed change that fixes it, or `—`.
- With no release yet, R4, R5 and R6 are `missing`, and their change is the first release.
- Mark each proposed change **I make this once you agree** (a local file) or **You run this** (a
  command that changes GitHub, given verbatim, ready to paste).
- Order the changes as they must happen. Immutability (L7) comes before the first tag push.
- End with the line that nothing has been changed, and ask which numbers the person agrees to.

## Make the agreed changes

Make only the changes the person agreed to, then show what you changed (`git status`, `git diff`).

### (a) No publishing repo yet

1. Create a new folder `murmur-artifacts/` and run `git init -b main` in it.
2. Lay it out as the tree in `references/layout.md` shows.
3. Copy `assets/release.yml` to `.github/workflows/release.yml`. For this layout no placeholder
   needs filling.
4. Ask the person which licence they want, and write it to `LICENSE`. Never pick one silently.
5. Move or write each artifact into its folder: `tools/<name>/`, `drivers/<name>/`, `hooks/<name>/`
   or `skills/<name>/`. Use the pointers in "What this is for" to build each one; check each with
   `mur build <folder> --output <scratch folder>/`.
6. Write a `README.md` saying what the repository serves and how to install it, then
   `git add -A && git commit -m "Set up murmur-artifacts"`.

Then hand the person these commands, in this order:

```bash
gh repo create <owner>/murmur-artifacts --public --source . --push
gh repo edit <owner>/murmur-artifacts --add-topic murmur-artifacts
gh api -X PUT repos/<owner>/murmur-artifacts/immutable-releases
git tag <name>-v<version> && git push origin <name>-v<version>
```

The third must run before the first tag push: a release published before immutability is on never
gets an attestation, and MAP lists it as mutable. The tag is in R6's form, one per artifact; push
the tags one at a time, waiting for each release workflow to finish.

### (b) A project repo that builds artifacts on the side

Leave the project repository untouched. Create the separate home repository exactly as in (a), in
a new folder beside the project, not inside it. In its `.github/workflows/release.yml`:

1. Uncomment the step marked SITUATION (b) that checks out the project repository, and fill in its
   `repository:` and its `ref:`, a tag or full commit SHA of the project, so a release always builds
   the same sources. Move the `ref:` to the project's new tag or commit for each release.
2. Add the checkout's path to `ARTIFACT_ROOTS`, as the comment shows, so the workflow finds the
   artifact folder in it and runs `mur build` on that folder there.
3. When the artifact packs a built component, uncomment the build step and fill in the project's
   own build command. That build has to run before `mur build` packs the folder; the pointers in
   "What this is for" cover building a component.

A skill needs no build. Copy its folder into the home repository's `skills/<name>/` instead, so
`npx skills add <owner>/murmur-artifacts --skill <name>` finds it there.

### (c) An existing `murmur-artifacts` repo

Apply each agreed change from the report, and nothing more. Make local file changes on a branch
when `main` must stay equal to the last release (L4), and hand the person the push and any other
command that changes GitHub.

### (d) A new version of a listed artifact

1. Ask for the new version, and confirm it is unreleased. This must answer `release not found`, and
   the second command must print nothing:

   ```bash
   gh release view <name>-v<new-version> --repo <owner>/murmur-artifacts
   git ls-remote --tags origin <name>-v<new-version>
   ```

2. Work on a branch, so `main` stays equal to the last release (L4) until this one is published:
   `git switch -c release/<name>-v<new-version>`.
3. Change the version in the artifact's own manifest only: `version:` in its `murmur.yaml`, or
   `metadata.version` in its `SKILL.md` front block. Make the other agreed changes, then check with
   `mur build <folder> --output <scratch folder>/`, which must write
   `<name>-<new-version>.mur.zip`, and commit.
4. Hand the person the push of the branch and the tag, in R6's form:

   ```bash
   git push origin release/<name>-v<new-version>
   git tag <name>-v<new-version> && git push origin <name>-v<new-version>
   ```

5. After the release is published, the person brings `main` up to it:

   ```bash
   git switch main && git merge --ff-only <name>-v<new-version> && git push origin main
   ```

Never touch an existing release or tag. A mistake in a released version is fixed by a newer version.

## Confirm (the person runs these)

Once the release workflow has finished, hand the person these checks, with the real owner, scope,
name, version and tag filled in:

1. **The release is immutable and attested.** `gh release verify <tag> --repo <owner>/murmur-artifacts`
   passes, and `gh release view <tag> --repo <owner>/murmur-artifacts --json isImmutable,isDraft`
   prints `"isImmutable":true` and `"isDraft":false`.
2. **The index lists the version.**
   `mur search "" --index https://github.com/<owner>/murmur-artifacts/releases/latest/download/artifacts-index.json`
   shows a `<scope>/<name>` row at the new version.
3. **It installs.** With the source in `~/.murmur/config.yaml`:

   ```yaml
   registry:
     sources:
       - repo: <owner>/murmur-artifacts
   ```

   `mur install <scope>/<name>@<version>` succeeds.
4. **A skill installs from git.** Once `main` holds it,
   `npx skills add <owner>/murmur-artifacts --skill <name> --list` finds it.
5. **MAP lists it.** After the next nightly crawl, at midnight US Eastern time, the artifact shows at
   `https://map.murmur.nexus/<scope>/<name>`, or in `https://map.murmur.nexus/facts.json`.
