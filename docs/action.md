# The zup GitHub Action

```yaml
- uses: orielhaim/zup@action-v1
  with:
    operation: build
```

That is the whole thing. The action installs the zup CLI, verifies it, runs the
phase you asked for, and reports the result as annotations, a job summary and
typed outputs.

## What it is, and what it is not

The action is an **orchestration and reporting layer**. Every decision about what
to build, how to compose it, what to attest and how to publish a release is made
by the Rust CLI, and the action's job is to make the CLI pleasant to call from a
workflow.

```text
you write          the action does                       the CLI decides
─────────────────────────────────────────────────────────────────────────────
operation: build   install + verify zup, run it,         how a target builds,
                   parse one JSON envelope,               what an artifact is,
                   annotate, summarise                    what is worth attesting
```

Three things it deliberately does not do:

- **It is not a second GitHub publisher.** Creating a release, uploading an asset,
  resuming a draft and verifying a remote SHA-256 are `zup publish github`, with
  fifty-two tests against a host that misbehaves. An action that reimplemented
  them would be a second publisher with a second set of bugs.
- **It is not framework-aware.** No Tauri, Electron or Flutter logic. When a
  future adapter produces `latest.json`, a `.sig`, a `latest.yml` or a
  `.blockmap`, they appear in the release manifest and the action publishes and
  attests them without a line changing.
- **It does not parse human output.** It runs `zup --format json` and reads one
  versioned envelope. The same envelope is what a future Tauri adapter, an
  Electron adapter, or a CI system that is not GitHub would read.

## Why a JavaScript action

Four options were considered. The decision is recorded here so it does not need
revisiting casually.

| option | verdict |
| --- | --- |
| `actions-rs` 0.1.1 | Rejected. The organisation was archived in October 2023 and its actions run on a Node version GitHub no longer supports. A release pipeline is the worst place to depend on it. |
| `ghactions` 0.20 | Rejected. It is maintained, and that is the strongest argument for it. But a Rust action is distributed as a compiled binary per platform, so the project would ship six binaries whose only job is to download one more binary. |
| **JavaScript/TypeScript** | **Chosen.** The GitHub-native model is cross-platform, Marketplace and self-hosted-runner behaviour is well understood, and `node24` is the current runtime. |
| composite / Docker | Rejected. A composite action cannot express a package manager, a cache, or a binary verification. A Docker action needs a container per architecture and adds a layer between the runner and the tool. |

Cross-cutting, the deciding arguments: the action's entire job is to install a
native `zup`, so a native action that only installs a native binary is a native
binary with fewer features. The `@actions/*` packages *are* the API — inputs,
outputs, log groups, masking, annotations, job summaries, the tool cache and
artifact uploads are all toolkit functions, and reimplementing them means
reimplementing a workflow runtime. And `action.yml` with `runs.using: node24` is
the shape the runner, the Marketplace and dependabot all understand.

Dependencies are pinned to exact versions with a committed `bun.lock`, verified
against the npm registry at implementation time rather than copied from here:

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

**1. `Bun` is not in scope for the action's source.** `tsconfig.json` — the one
covering `action/src` — sets `"types": ["node"]` and nothing else, so a
`Bun.file` or `Bun.spawn` in shipped code is a **compile error** rather than a
review comment. The tests get Bun's globals through a separate
`tsconfig.test.json`, because they run under Bun. That is why there are two
tsconfigs rather than one.

**2. The bundle is checked for Bun APIs anyway.** Type checking does not cover a
transitive dependency that reaches for `Bun.something` at import time.
`scripts/verify-runtime.mjs` fails on any `Bun.*` in the artifact.

**3. The bundle is executed under Node.** `bun build --target=node` produces Node
code and `bun test` proves the logic, but a bundle Bun accepts and Node cannot
load is a green CI run and a broken release in somebody else's workflow. So
`verify-runtime.mjs` *runs the built artifact as a child process* and inspects
what it wrote.

`bun build` replaced esbuild. Against this dependency tree it bundles all of it —
1 221 modules, including the Sigstore/protobuf stack — in about 120 ms with no
shims and no configuration. The one thing it does *not* do is emit a chunk graph,
discussed under [Performance](#performance).

> **A `bun build` trap.** On Bun 1.4.2, `outfile` is accepted and **ignored**: the
> artifact lands at `./main.js` rather than the path `action.yml` names, and every
> step reports success. `scripts/build.mjs` uses `outdir` with an explicit
> `naming.entry`, then asserts the file exists where `action.yml` expects it.

## Where it lives, and how it is versioned

`action.yml` is at the repository root and the implementation is in `action/`, so
the action and the CLI ship from one commit and a released action defaults to the
exact zup version it was built against.

The action has its own tag namespace, `action-v*`, because `v1` alone would put two
unrelated meanings on one prefix.

```text
action-v1     a movable tag at the current action
```

One tag, not a version ladder. Nothing outside this repository consumes the action
yet, so there is no compatibility to promise and no reason to spend a major on a
breaking input or output change. When somebody does depend on it, the tag is the
place to start versioning; until then a second tag would be bookkeeping for a
promise nobody asked for.

The action is not published. Its tag exists so a workflow can name it, not so a
release exists.

## The generated workflow is optional

There are now two equally valid ways to release, and this milestone is the reason
the second one exists.

**Guided.** `zup ci github generate` writes a complete pipeline: plan, build
matrix, compose, attest, publish. Use it when you want a working release and do
not want to think about the architecture.

**Composable.** Write the steps yourself with repeated invocations of this action.
Use it when you want to control the matrix, the runner per target, or the order of
the phases.

The generated file calls this action once per phase rather than collapsing the
pipeline into one step. A release pipeline is the file a project most needs to
audit, and one job per phase keeps `plan → build → compose → attest → publish`
readable in the diff.

## Operations

| operation | what it runs |
| --- | --- |
| `setup` | nothing — installs zup and puts it on `PATH` |
| `build` | `zup build` |
| `compose` | `zup publish stage`, folding per-target output into one release |
| `attest` | reads the release manifest and attests the final bytes it names |
| `publish` | `zup publish github` |
| `release` | build → compose → attest → publish, in that order |

`release` is a loop over the same phases a hand-written pipeline runs, not a
separate code path. Attestation is only part of it when `attest: true` is set;
`operation: attest` already implies it.

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
process listing and in `ps` output on a shared runner.

It is registered with `core.setSecret` before anything can print it, and the
action's own log redacts every registered secret from every message it writes:
annotations, outputs, the job summary and failures.

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

A build needs nothing.

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
  "code": "zup_manifest::unknown_target_profile_reference",
  "message": "resource references unknown target profile `x64`",
  "help": "use an exact profile id declared under [build.targets]",
  "source": { "file": "zup.toml", "startLine": 12, "startColumn": 3, "endLine": 12, "endColumn": 9 }
}
```

becomes an error annotation on `zup.toml` at line 12, visible in the workflow's
file view. miette's rendering is not reformatted; the action only decides which
level and which region. Human terminal output is still streamed and grouped for
whoever is reading the log.

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
| `release-manifest` | the release manifest, relative to `project-path` |
| `release-id` | the GitHub release id |
| `release-url` | the release page |
| `publish-receipt` | the publisher's receipt, relative to `project-path` |

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
it larger. Note that the service then names the artifact after the file rather
than after `workflow-artifact-name`; the log says so when that happens, because a
matrix that needs to distinguish two artifacts must give the *files* distinct
names.

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
and it is the reason a bootstrap dependency on a published release does not exist.

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

### Local development

`@github/local-action` runs the action on a workstation. It emulates a subset of
the toolkit, so it is a smoke test rather than a substitute: the unit tests are
the real coverage, and they need no runner and no network.

```bash
cd action
bun run local
```

### What CI checks

`action.yml` runs three jobs. `check` is the gate:

```text
bun install --frozen-lockfile
bun run typecheck          # src with no Bun types; tests with them
bun run biome              # lint and format
bun test                   # 143 unit tests
bun run build              # one ESM file
git diff --exit-code -- action/dist
bun run metadata           # action.yml matches the implementation
node scripts/verify-runtime.mjs    # the bundle, under Node 24
```

The last line is the one that cannot be skipped and is also the one most likely
to be deleted by somebody who does not know what it is for. It executes the built
artifact as a child process and asserts that it is a single ESM file, contains no
`Bun.*` API, bakes in no absolute path from the build machine, resolves its
`zup-path` input, and writes a job summary. Each of those checks was verified to
fire by breaking the bundle deliberately.

### Updating the action's own GitHub Action dependencies

The repository's workflows use version refs — `actions/checkout@v7` — and
`github-actions.lock.json` tracks what each ref resolves to. Dependabot advances the
refs; the lock records the answer.

```bash
cargo xtask github-action-pins check             # offline; runs in CI
cargo xtask github-action-pins check --online    # also resolve against upstream
cargo xtask github-action-pins refresh           # advance series, re-record commits
```

`check` runs in every CI job, is offline by default, and fails the build. It
verifies lock syntax, that every recorded commit is a full lowercase SHA, that every
action a generated workflow needs is present, and that every `uses:` in a committed
workflow is the ref the lock tracks. A workflow bumped to `v8` by hand in one file
while the lock still says `v7` is drift, and it is caught here rather than six months
later.

`--online` answers the two questions a version ref cannot:

- Is a **newer major series** published? (`@v7` while `v8` exists.)
- **Does this ref still point where it did?** A tag can be moved, and a moved tag is
  invisible in a diff. The lock's recorded commit is the only thing that makes the
  difference between "unchanged" and "somebody moved a tag" visible. `check --online`
  reports a moved *tag*; a moved *branch* channel such as `dtolnay/rust-toolchain@stable`
  is the channel working as designed and is not reported, because a report that fires
  every week trains people to ignore it.

`refresh` reaches GitHub, advances every tracked series to the newest major, and
re-records the commit each ref resolves to. It never runs inside a build: a ref that
moved under a developer who was only regenerating a matrix is a supply-chain change
they did not make. Resolution uses `git ls-remote`, so it needs no token, works
against a mirror, and adds no HTTP client to a tool that otherwise has none.

This finding is not hypothetical. `Swatinem/rust-cache@v2` had moved between the
lock being written and `--online` being run, and the check reported it. A workflow
reading `@v2` was running a different commit than the one that had been reviewed.

`zup ci github check --format json` reports the same refs, with the commit each
resolved to and the `checkedAt` date, and marks which ones a *generated* workflow
uses — so a project is not told to track `Swatinem/rust-cache`, which is zup's own CI
and not its pipeline's.

Dependabot opens the pull requests that keep `action/bun.lock` and the
repository's workflows current. It does not touch generated workflow fixtures, so
a bot cannot fight the generator.

## Migration note

Workflows generated by the previous milestone used:

- `cargo build --release -p zup --all-features --bin zup` in every job, which only
  ever worked inside the zup repository itself;
- `dtolnay/rust-toolchain` and `Swatinem/rust-cache` in a consumer's pipeline,
  which the generated file did not need;
- `actions/checkout@v5`, `actions/upload-artifact@v4` and
  `actions/download-artifact@v5` as floating tags;
- `actions/attest-build-provenance`, which is now only a wrapper on top of
  `actions/attest`;
- `GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}` as a step-level `env:`, which put
  the credential in the environment of the job it shared.

Re-run the generator and review the diff:

```bash
zup ci github generate --force
```

The phase structure, the least-privilege permissions, the native runner labels and
`cancel-in-progress: false` are all unchanged. What changes is that each phase calls
the action, no job compiles zup, every third-party action sits on the ref the lock
tracks, the attestation phase uses `actions/attest`, and the credential is an input
to one step rather than an environment variable on several.

## Performance

Measured on this repository, Node 24.15.0, Windows.

| | |
| --- | --- |
| action startup, median of 10 | **70 ms** |
| bare `node -e 0` on the same machine, median of 10 | 42 ms |
| **overhead the action adds** | **~28 ms** |
| bundle size | 4.4 MiB, one file |
| `bun build` wall time | ~120 ms |
| `bun test` wall time (143 tests) | ~145 ms |

~28 ms is the number that matters, and it is small enough not to be worth
optimising further: a build that takes four minutes does not notice, and the
alternative — hand-rolling inputs, annotations and the tool cache — would cost
more in maintenance than it saves in milliseconds.

### Why 4.4 MiB in one file

The bundle is a single file rather than a file plus lazily-loaded chunks, and it
is roughly 1.6× larger than a split build would be. Sigstore (807 KiB) and the
artifact service's Twirp client (958 KiB) are reachable only when a workflow sets
`attest: true` or `upload-workflow-artifacts: true`, and code splitting would keep
them out of the parse path of a build that does neither.

The trade is deliberate:

- **A single file cannot half-exist.** A chunk graph has content-hashed names, so
  one missing file is an `ERR_MODULE_NOT_FOUND` on a runner, after a green build,
  in somebody else's release workflow. `verify-runtime.mjs` asserts `dist/`
  contains exactly one file for the same reason.
- **A single file is reviewable.** The bundle is committed. A graph of
  content-hashed chunks is a diff nobody reads.
- **Parsing 4.4 MiB costs about 28 ms**, inside the noise of any real build. The
  split would save a fraction of that and cost reviewability.

If startup ever matters — a matrix of 200 short jobs, say — the split is a
one-line change to `scripts/build.mjs` plus a loop in `verify-runtime.mjs`.

**Not yet measured:** the cold download and the warm tool-cache resolution. Both
need a published zup release, and there is not one yet. The opt-in
`tool-bootstrap` job is where those numbers belong the day there is.

## Limitations

- **A published zup release is required for the download path.** The
  repository's own action tests use `zup-path` with a locally built zup, so testing
  the action does not require having released it. The opt-in `tool-bootstrap` job
  covers the real path once a release exists.
- **Inputs and outputs change freely.** Nothing depends on them yet, and the
  `action-v*` namespace keeps them off the CLI's tag prefix.
- **The bundle is one 4.4 MiB file.** A deliberate trade for reviewability and for
  a build that cannot half-exist. See [Performance](#performance).
- **`@actions/attest` carries an unfixable advisory.** `actions/attest` v4 is the
  current GitHub guidance and this is the supported way to reach it; the
  alternative is reimplementing Sigstore and OIDC, which is strictly worse.
- **`operation: release` runs in one job.** A multi-job pipeline is a
  hand-written workflow; the generated one shows how, and the action is
  composable into it.
- **Attestations need a plan that supports them.** A requested attestation that
  fails on a plan without attestation support is an error, because a release that
  looks attested and is not is worse than one that failed.
