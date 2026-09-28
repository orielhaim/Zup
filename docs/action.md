# The zup GitHub Action

```yaml
- uses: orielhaim/zup@action-v1
  with:
    operation: build
```

That is the whole thing. The action installs the zup CLI, verifies it, runs the
phase you asked for, and reports the result as annotations, a job summary and
typed outputs.

`action.yml` declares every input and output with its default. The ones with
behaviour worth knowing about are covered below.

## What it is, and what it is not

The action is an **orchestration and reporting layer**. Every decision about what
to build, what to attest and how to publish a release is made by the Rust CLI.

- **It is not a second GitHub publisher.** Creating a release, uploading an
  asset, resuming a draft and verifying a remote SHA-256 are `zup publish
  github`; see [GitHub distribution](github-distribution.md). An action that
  reimplemented them would be a second publisher with a second set of bugs.
- **It is not framework-aware.** No Tauri, Electron or Flutter logic. When an
  adapter produces `latest.json`, a `.sig`, a `latest.yml` or a `.blockmap`, they
  appear in the release manifest and the action publishes and attests them
  without a line changing.
- **It does not parse human output.** It runs zup with `--format jsonl` and reads
  the versioned protocol stream, which is also what a CI system that is not GitHub
  reads; see [the automation protocol](automation.md).

## Where its types come from

The action has no hand-written copy of zup's result envelope. Its declarations are
generated from the same Rust DTOs the JSON Schema is generated from:

```text
crates/zup-automation/            the contract
schema/automation-v1.schema.json  for a consumer that is not TypeScript
action/src/protocol.generated.ts  what the action imports
fixtures/automation/*             golden documents the action's tests read
```

```bash
cargo xtask automation generate   # regenerate after changing a DTO
cargo xtask automation check      # CI fails when they drift
```

The decoder in `action/src/protocol.ts` is hand-written, on purpose: it is where the
ignore-what-you-do-not-know rule lives, and a generated decoder would know none of
it. What it implements is three rules — refuse a different protocol major, ignore
what you do not know, and refuse a document that says nothing usable — and they are
in [the automation protocol](automation.md) because they are the contract rather than
this consumer's private policy.

The action's own types are the exceptions, and they are named as such: which
*workflow step* ran, what to attest, and where the workflow artifact went. `attest`
in particular has no zup operation at all — see [Operations](#operations).

## Why a JavaScript action

JavaScript on `node24` is the GitHub-native model, and `action.yml` with
`runs.using: node24` is the shape the runner, the Marketplace and dependabot all
understand. A Rust action would ship six compiled binaries whose only job is to
download one more binary, and the `@actions/*` packages *are* the API: inputs,
outputs, masking, annotations, job summaries, the tool cache and artifact
uploads are all toolkit functions.

Dependencies are pinned to exact versions in a committed `action/bun.lock`:

| package | version | why |
| --- | --- | --- |
| `@actions/core` | 3.0.1 | inputs, outputs, annotations, summaries, masking |
| `@actions/exec` | 3.0.0 | running zup with an argument vector |
| `@actions/tool-cache` | 4.0.0 | the runner's own tool cache |
| `@actions/artifact` | 6.2.1 | workflow artifact upload |
| `@actions/attest` | 3.2.0 | Sigstore-backed provenance |
| `@github/local-action` | 7.0.1 | running the action locally, dev-only |
| `typescript` | 7.0.2 | type checking |
| `@biomejs/biome` | 2.5.14 | formatting and linting |

> **One known advisory, and why it is acceptable.** `@actions/attest` 3.2.0 depends
> on `@sigstore/core` ≤ 3.2.0, which has a DSSE payload-type binding advisory with
> no fixed release. A workflow that does not set `attest: true` never reaches that
> code.

## The toolchain split

```text
Bun  ── installs, tests, bundles ──▶  action/dist/index.js  ── runs on ──▶  Node 24
1.4.2                              one ESM file, no node_modules          GitHub Actions runtime
```

Three gates keep the two runtimes from blurring:

1. **Bun is not in scope for the action's source.** `tsconfig.json` — the one
   covering `action/src` — sets `"types": ["node"]` and nothing else, so a
   `Bun.file` or `Bun.spawn` in shipped code is a **compile error** rather than a
   review comment. The tests get Bun's globals through a separate
   `tsconfig.test.json`, which is why there are two tsconfigs rather than one.
2. **The bundle is checked for Bun APIs anyway.** Type checking does not cover a
   transitive dependency that reaches for `Bun.something` at import time.
   `scripts/verify-runtime.mjs` fails on any `Bun.*` in the artifact.
3. **The bundle is executed under Node.** `bun build --target=node` produces Node
   code and `bun test` proves the logic, but a bundle Bun accepts and Node cannot
   load is a green CI run and a broken release in somebody else's workflow. So
   `verify-runtime.mjs` *runs the built artifact as a child process* and inspects
   what it wrote.

> **A `bun build` trap.** On Bun 1.4.2, `outfile` is accepted and **ignored**: the
> artifact lands at `./main.js` rather than the path `action.yml` names, and every
> step reports success. `scripts/build.mjs` uses `outdir` with an explicit
> `naming.entry`, then asserts the file exists where `action.yml` expects it.

## Where it lives, and how it is versioned

`action.yml` is at the repository root and the implementation is in `action/`, so
the action and the CLI ship from one commit and a released action defaults to the
exact zup version it was built against.

The action has its own tag namespace, `action-v*`, because `v1` alone would put
two unrelated meanings on one prefix. It is one movable tag, not a version
ladder: nothing outside this repository consumes the action yet, so there is no
compatibility to promise. The tag exists so a workflow can name it, not so a
release exists.

## The generated workflow is optional

There are two equally valid ways to release. `zup ci github generate` writes a
complete pipeline — plan, build matrix, compose, attest, publish — for a project
that wants a working release without designing the architecture. Writing the
steps yourself with repeated invocations of this action is for a project that
wants to own the matrix, the runner per target, or the order of the phases.

The generated file calls this action once per phase rather than collapsing the
pipeline into one step, because one job per phase keeps `plan → build → compose →
attest → publish` readable in the diff.

## Operations

| operation | what it runs |
| --- | --- |
| `setup` | nothing — installs zup and puts it on `PATH` |
| `build` | `zup build` |
| `compose` | `zup publish stage`, folding per-target output into one release |
| `finalize` | `zup sign verify`, then reads the release description it rewrote |
| `attest` | reads the release manifest and attests the final bytes it names |
| `publish` | `zup publish github` |
| `release` | build → compose → finalize → attest → publish, in that order |

`release` is a loop over the same phases a hand-written pipeline runs, not a
separate code path. Attestation is only part of it when `attest: true` is set;
`operation: attest` already implies it.

**A phase is a workflow step; an operation is what zup said it did.** The two
vocabularies are not the same, and the translation is one table in
`action/src/phases.ts`:

| phase | zup operation |
| --- | --- |
| `build` | `build` |
| `compose` | `publish.stage` |
| `finalize` | `sign.verify` |
| `attest` | *none* |
| `publish` | `publish.github` |

`attest` has no operation because zup does not talk to Sigstore. The OIDC token
exchange is GitHub's and the signature format is Sigstore's; what zup owns is which
bytes are worth attesting, and it says so in the release description, which the
action reads itself. A `zup attest` verb would be a second signer with a second set
of bugs.

## Examples

### Minimal build

```yaml
name: build
on: [push]

jobs:
  build:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v7
      - uses: orielhaim/zup@action-v1
```

`operation: build` is the default.

### One target

```yaml
- uses: orielhaim/zup@action-v1
  with:
    operation: build
    target: windows-x64
```

`target` takes a profile name from `zup.toml`, not a target triple. A profile is
the name in the file a reader is already looking at.

### Release to GitHub

```yaml
name: Release

on:
  push:
    tags: ["v*"]

permissions:
  contents: write

jobs:
  release:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v7
      - uses: orielhaim/zup@action-v1
        with:
          operation: release
```

### Release with provenance

```yaml
permissions:
  contents: write
  id-token: write       # exchange an OIDC token for a signing identity
  attestations: write   # store the attestation

steps:
  - uses: actions/checkout@v7
  - uses: orielhaim/zup@action-v1
    with:
      operation: release
      attest: true
```

The subjects come from the release manifest, not from a glob, and the attestation
is of the final bytes — after any signing, before publication. A requested
attestation that cannot be created is an error, not a skip.

### Setup only

```yaml
- uses: orielhaim/zup@action-v1
  with:
    operation: setup

- run: zup doctor
- run: zup check
```

### A hand-written matrix

```yaml
jobs:
  build:
    runs-on: ${{ matrix.runner }}
    strategy:
      fail-fast: false
      matrix:
        include:
          - profile: x64
            runner: windows-latest
          - profile: arm64
            runner: windows-11-arm
    steps:
      - uses: actions/checkout@v7
      - uses: orielhaim/zup@action-v1
        with:
          operation: build
          target: ${{ matrix.profile }}
          release-dir: dist/variants/${{ matrix.profile }}
          upload-workflow-artifacts: true
          workflow-artifact-name: variant-${{ matrix.profile }}
          artifact-retention-days: 7

  compose:
    needs: build
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v7
      - uses: actions/download-artifact@v8
        with:
          pattern: variant-*
          path: dist/variants
          merge-multiple: true
      - uses: orielhaim/zup@action-v1
        with:
          operation: compose
          release-dir: dist
          upload-workflow-artifacts: true
          workflow-artifact-name: compose

  publish:
    needs: compose
    runs-on: ubuntu-latest
    permissions:
      contents: write
    steps:
      - uses: actions/checkout@v7
      - uses: actions/download-artifact@v8
        with: { pattern: compose, path: dist }
      - uses: orielhaim/zup@action-v1
        with:
          operation: publish
          release-dir: dist
          github-token: ${{ secrets.GITHUB_TOKEN }}
```

## Installing the CLI

```text
explicit zup-path
  → requested zup-version, from the tool cache
  → download exactly that version
```

There is no fourth step. The action does not fall back to a `zup` on `PATH`,
because a runner image's `zup` is whatever somebody installed weeks ago and a
release that silently used it would be a release nobody reproduced.

**The default version is the one this action was tested against**, compiled in
and carried by the release. A workflow that pins an action ref therefore gets
reproducible tool behaviour. Override it with `zup-version`.

Runner support is `windows/x64`, `windows/arm64`, `linux/x64`, `linux/arm64`,
`macos/x64` and `macos/arm64`. An architecture outside that set is refused with a
message naming the alternatives, before any network access.

### Verification

An executable is never run because a URL resolved. Before anything is made
executable, the action compares the downloaded bytes against **two independent
records**:

1. the SHA-256 zup recorded when it published, read from the release's own
   `zup-release.json` — the same format `zup publish github` already writes, so
   nothing new was invented for CI;
2. the SHA-256 GitHub computed for the uploaded asset, when the API reports one.

A size mismatch, either digest mismatch, or a truncated response all refuse. The
same check runs against a cache entry, because a half-written cache directory, a
full disk, or two concurrent jobs sharing a cache all produce a file of the right
name and the wrong bytes. A corrupt entry is re-downloaded rather than executed.

`zup-path` skips all of it. That is the point of it: a locally built zup has no
published digest to check against.

## The security model

### The token is in one subprocess, and only one

`zup build` may run Tauri, Electron, Cargo build scripts and npm scripts. A token
in that environment is a token handed to whatever the project's build does, which
is arbitrary code. So:

```yaml
- uses: orielhaim/zup@action-v1
  with:
    operation: release
    github-token: ${{ secrets.GITHUB_TOKEN }}
```

runs, in order:

| phase | `GH_TOKEN` / `GITHUB_TOKEN` in its environment |
| --- | --- |
| `zup build` | **absent** |
| `zup publish stage` | **absent** |
| attestation | **absent** |
| `zup publish github` | present |

This is not "the token is scrubbed from the others". Every subprocess environment
is **built from scratch** from an allowlist, so a variable GitHub adds next year
is not in a zup build by default. The tests assert the allowlist structurally, not
by grepping a log line.

The token is also never passed as a CLI argument — arguments are visible in a
process listing and in `ps` output on a shared runner. It is registered with
`core.setSecret` before anything can print it, and the action's own log redacts
every registered secret from every message it writes: annotations, outputs, the
job summary and failures.

### Dangerous triggers

`pull_request_target` and `workflow_run` run with the base repository's secrets
and write token while executing code that may have come from a fork. Publishing
there hands a release credential to whoever opened the pull request.

The action **refuses** `publish` and `release` on those events, and produces an
error annotation explaining why. `build`, `compose`, `setup` and `attest` are not
refused: they write nothing to the repository, and a project that only builds on
`pull_request_target` has a legitimate reason to.

The safe pattern is a different workflow, not a flag:

```text
pull_request_target  builds the untrusted code, uploads artifacts, holds no write token
workflow_run         downloads those artifacts, publishes with a write token
```

If a project cannot restructure today:

```yaml
- uses: orielhaim/zup@action-v1
  with:
    operation: release
    allow-unsafe-publish: true
```

which emits a warning annotation even when the run proceeds. The escape hatch is
refused outright when the checkout came from a fork, because that is a decision
about a specific stranger's branch rather than about a configuration.

### Permissions

| operation | permissions |
| --- | --- |
| `setup`, `build`, `compose` | none |
| `attest` | `id-token: write`, `attestations: write` |
| `publish`, `release` | `contents: write` |
| `publish --dry-run` | `contents: read` |

A dry run still needs a credential, because it verifies against the live
repository. It does not need write permission.

## Diagnostics become annotations

zup emits structured diagnostics; the action maps them onto the runner's
annotation commands, including source positions:

```json
{
  "severity": "error",
  "code": "zup.manifest.unknown_target",
  "message": "resource references unknown target profile `x64`",
  "help": "use an exact profile id declared under [build.targets]",
  "source": { "file": "zup.toml", "start_line": 12, "start_column": 3, "end_line": 12, "end_column": 9 }
}
```

becomes an error annotation on `zup.toml` at line 12, visible in the workflow's
file view. A relative path is resolved against `project-path`, because GitHub
resolves an annotation path against the workspace root and an annotation on a path
that does not resolve is invisible.

A diagnostic arrives twice — streamed as it is found, and again in the final
result — and is annotated **once**. Telling the reader the same fact twice is worse
than not telling them at all.

A warning on a successful result is a warning annotation, not a failure.
`zup check` reporting that two targets cannot be composed into one artifact is a
fact about how the release has to be built, not a broken project; see
[severity and status](automation.md#severity-and-status-agree).

## A failing zup is read, not scraped

When zup exits nonzero, the action reads the failure **out of the protocol document**
rather than out of stderr:

```text
zup publish github --dry-run  →  exit 1
                              →  the stream's last line is a `completed` event
                              →  its result says `status: "failure"` and carries
                                 the diagnostic with its code and location
```

Every diagnostic in that result is annotated, the phase is reported as failed, and
the summary keeps whatever was produced before the failure — a build that composed
four artifacts and then failed to publish is the case somebody most needs a record
of.

This is why the phases run `--format jsonl` rather than `--format json`. The stream
also gives the action something to read *while* zup is working; the exit code and
the document are read together, which is the only way they can be made to agree.

## Logs and the job summary

One log group per phase — `Setup zup`, `Build`, `Compose`, `Attest`, `Publish` —
and nothing nested inside them. `RUNNER_DEBUG` raises zup's own verbosity and
prints the exact argument vector, so a debug run is reproducible by hand.

The job summary is a record, not a log excerpt:

```markdown
## zup

| Version | 1.4.0 |
| Targets | windows-x64, windows-arm64 |
| Artifacts | 2 |

| zup CLI | 1.4.0 (windows-x64, cache) |

| Artifact | Size | SHA-256 |
| --- | ---: | --- |
| `Acme-Windows-Setup.exe` | 237 MiB | `3b1f…` |
| `Acme-Web-Setup.exe` | 2.0 MiB | `9ac4…` |

| Release | `v1.4.0` |
| Repository | acme/acme |
| Status | published |
| Immutable | yes |
| URL | https://github.com/acme/acme/releases/tag/v1.4.0 |
```

No internal JSON, and no timestamps: two runs of the same build produce the same
summary, which is what makes it reviewable.

## Outputs

| output | value |
| --- | --- |
| `zup-path` | absolute path to the zup executable used |
| `zup-version` | the zup version that was installed or used |
| `app-version` | the application's version |
| `artifact-paths` | JSON array of `{path, size, digest}` |
| `release-manifest` | the release description, relative to `project-path` |
| `release-id` | the provider's release reference |
| `release-url` | the release page |
| `publish-receipt` | the publisher's receipt, relative to `project-path` |

`release-id` is a string, not a number. It is the provider's own reference, and
whether it fits in a JavaScript `number` is the provider's business — GitHub's are
19-digit snowflakes and will not, indefinitely.

`artifact-paths` is JSON rather than a newline-joined list because a caller that
needs to iterate needs to parse, and JSON survives a path with a space in it.
Manifests are referenced by path, not inlined: an output containing 400 MiB of
artifact list is an output nobody reads.

## Advanced arguments

```yaml
- uses: orielhaim/zup@action-v1
  with:
    operation: build
    args: --release-manifest "dist/my manifest.json" --force
```

`args` is tokenized into an argument vector. It is **never** concatenated into a
command string and no process is spawned with `shell: true`, so `;`, `|`, `&&`,
`$(...)` and backticks are ordinary characters with no meaning.

Quoting follows the platform, because the two families genuinely differ:

- POSIX: single quotes are literal, double quotes allow `\"`, a backslash escapes
  the next character.
- Windows: single and double quotes group, and a backslash is a *path separator*.
  `C:\temp\dist` is one argument containing no escapes, and treating `\` as an
  escape there would silently delete it. A single quote is literal, matching
  `CommandLineToArgvW`.

Arguments are appended last, so a typed input can be overridden from `args` when
a project genuinely needs it.

## Workflow artifacts

Off by default. A release can be gigabytes, and nobody expects a build step to
spend artifact storage without being asked.

```yaml
- uses: orielhaim/zup@action-v1
  with:
    operation: build
    upload-workflow-artifacts: true
    workflow-artifact-name: variant-x64
    artifact-retention-days: 7
```

A single already-compressed output — a `.exe`, a `.zup` transport package, a
`.tar.zst` — is uploaded **without a zip**, using the direct artifact support
GitHub added in 2026. Zipping a 237 MiB installer spends CPU and storage to make
it larger. The service then names the artifact after the file rather than after
`workflow-artifact-name`; the log says so when that happens, because a matrix
that needs to distinguish two artifacts must give the *files* distinct names.
`@actions/artifact` is loaded through a dynamic import, so a workflow that never
uploads never parses the artifact service's Twirp client.

## GitHub Enterprise

The action does not hardcode `github.com` or `api.github.com` for anything about
the project being built:

- workflow context comes from `GITHUB_SERVER_URL` and `GITHUB_API_URL`;
- the artifact URL is derived from the server URL, so a GHES run produces a GHES
  link;
- the publication is `zup publish github`, which already models the web, API and
  upload bases separately and honours Enterprise hosts.

Downloading the public zup CLI from GitHub.com is a **separate concern** and is
modelled separately: it targets zup's own public repository, and it never sends
the project's GHES `GITHUB_TOKEN` there. The CLI download is unauthenticated.

`@actions/artifact` v4 and later are not supported on GHES. The action reports an
upload failure honestly rather than pretending the artifact exists.

## Self-hosted runners

The action runs on `node24`, which implies Actions Runner **2.327.1 or newer**.
That is declared in `action.yml` as `runs.minimum` rather than left to be
discovered as a mysterious failure.

For a runner whose architecture zup does not publish for, build zup from source
and pass `zup-path`. That is the same input the repository's own action tests use,
and it is why testing the action does not require having released it.

## Maintaining the action

```bash
cd action
bun install --frozen-lockfile
bun run check          # typecheck, lint, test, bundle, metadata, Node 24 run
bun run check:dist     # prove the committed bundle matches the source
bun run local          # run the action on this machine
```

`action/dist/index.js` is committed, because GitHub runs an action from the
repository without installing anything. `bun run check:dist` rebuilds and fails on
a diff, and CI runs it, so a stale bundle cannot be committed unnoticed.

Adding an input means adding it to `action.yml` **and** to the input table in
`action/src/inputs.ts`. `bun run metadata` fails if the two disagree in either
direction — an input nothing reads, or a read input that is not declared — and it
also fails if the default zup version in `workflow.ts` has drifted from the
workspace version.

`bun run local` uses `@github/local-action`, which emulates a subset of the
toolkit. It is a smoke test rather than a substitute: the unit tests are the real
coverage, and they need no runner and no network.

`action.yml` runs three jobs. `check` is the gate:

```text
bun install --frozen-lockfile
bun run typecheck          # src with no Bun types; tests with them
bun run biome              # lint and format
bun test                   # 178 unit tests, over the Rust-generated fixtures
bun run build              # one ESM file
git diff --exit-code -- action/dist
bun run metadata           # action.yml matches the implementation
node scripts/verify-runtime.mjs    # the bundle, under Node 24
```

The last line executes the built artifact as a child process and asserts that it
is a single ESM file, contains no `Bun.*` API, bakes in no absolute path from the
build machine, resolves its `zup-path` input, and writes a job summary. It cannot
be skipped.

`bun test` is a compatibility gate, not just coverage. The protocol tests read the
golden fixtures from `fixtures/automation/` on disk — the documents zup's own Rust
serializes — so a change to a DTO that the action's decoder has not been taught
fails here, naming the field, rather than in somebody's release workflow.

`real-zup` then invokes the action as `uses: ./` on `windows-latest` and
`windows-11-arm`, against a zup **built from this commit** through `zup-path`. It
covers setup, the CLI on `PATH`, a successful operation and its outputs, a project
path containing a space, a summary that carries the result without dumping the
document, and a failing zup failing the step with a diagnostic *code* in the
summary.

Real zup, not a stub, and that is the whole point. A stub that emits a hand-written
document proves the action against a document somebody typed — which is how the
action and the CLI came to disagree about the shape of a result in the first place.
Building from this commit also means the protocol under test is the one in the tree,
not the one in the newest release.

`tool-bootstrap` is opt-in because it needs a published zup release and reaches the
network. It covers the one path a `zup-path` binary cannot: the download, its
digest verification, and the tool cache.

### GitHub Action dependency pins

The repository's workflows use version refs — `actions/checkout@v7` — and
`github-actions.lock.json` tracks what each ref resolves to. Dependabot advances
the refs; the lock records the answer.

```bash
cargo xtask github-action-pins check             # offline; runs in CI
cargo xtask github-action-pins check --online    # also resolve against upstream
cargo xtask github-action-pins refresh           # advance series, re-record commits
```

`check` runs in every CI job, is offline by default, and fails the build. It
verifies lock syntax, that every recorded commit is a full lowercase SHA, that
every action a generated workflow needs is present, and that every `uses:` in a
committed workflow is the ref the lock tracks. A workflow bumped to `v8` by hand
in one file while the lock still says `v7` is drift, and it is caught here rather
than six months later.

`--online` answers the two questions a version ref cannot:

- Is a **newer major series** published? (`@v7` while `v8` exists.)
- **Does this ref still point where it did?** A tag can be moved, and a moved tag
  is invisible in a diff. A moved *tag* is reported; a moved *branch* channel such
  as `dtolnay/rust-toolchain@stable` is the channel working as designed and is
  not, because a report that fires every week trains people to ignore it.

`refresh` reaches GitHub, advances every tracked series to the newest major, and
re-records the commit each ref resolves to. It never runs inside a build: a ref
that moved under a developer who was only regenerating a matrix is a supply-chain
change they did not make. Resolution uses `git ls-remote`, so it needs no token,
works against a mirror, and adds no HTTP client to a tool that otherwise has none.

`zup ci github check --format json` reports the same pins as part of a larger
report; see [GitHub distribution](github-distribution.md).

Dependabot opens the pull requests that keep `action/bun.lock` and the
repository's workflows current. It does not touch generated workflow fixtures, so
a bot cannot fight the generator.

## The bundle

`action/dist/index.js` is one 4.4 MiB file rather than a file plus lazily-loaded
chunks. Sigstore (807 KiB) and the artifact service's Twirp client (958 KiB) are
reachable only when a workflow sets `attest: true` or
`upload-workflow-artifacts: true`, and code splitting would keep them out of the
parse path of a build that does neither. The trade is deliberate:

- **A single file cannot half-exist.** A chunk graph has content-hashed names, so
  one missing file is an `ERR_MODULE_NOT_FOUND` on a runner, after a green build,
  in somebody else's release workflow. `verify-runtime.mjs` asserts `dist/`
  contains exactly one file for the same reason.
- **A single file is reviewable.** The bundle is committed. A graph of
  content-hashed chunks is a diff nobody reads.
- **Parsing 4.4 MiB is cheap** next to any real build. The split would save a
  fraction of that and cost reviewability.

If startup ever matters — a matrix of 200 short jobs, say — the split is a
one-line change to `scripts/build.mjs` plus a loop in `verify-runtime.mjs`.

## Limitations

- **A published zup release is required for the download path.** The
  repository's own action tests use `zup-path` with a locally built zup, and the
  opt-in `tool-bootstrap` job covers the real path once a release exists.
- **`operation: release` runs in one job.** A multi-job pipeline is a
  hand-written workflow or the generated one; the action is composable into
  either.
- **Attestations need a plan that supports them.** A requested attestation that
  fails on a plan without attestation support is an error, because a release that
  looks attested and is not is worse than one that failed.
