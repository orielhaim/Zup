# The zup GitHub Action

```yaml
- uses: orielhaim/zup@action-v1
  with:
    operation: release
```

The action installs the zup CLI, verifies it, runs the phase you asked for, and
reports the result as annotations, a job summary and typed outputs. Every
decision about what to build and how to publish is made by the Rust CLI.

It is **not** a second GitHub publisher (that is `zup publish github`), **not**
framework-aware (a Tauri `latest.json` appears in the release manifest and is
published without a line changing), and it **does not parse human output** - it
reads the versioned protocol stream.

## What it is not

An action that reimplemented release publication would be a second publisher
with a second set of bugs. `attest` has no zup operation because zup does not
talk to Sigstore: what zup owns is *which bytes are worth attesting*, and it
says so in the release description, which the action reads itself.

## Operations

| operation | what it runs |
| --- | --- |
| `setup` | nothing; installs zup and puts it on `PATH` |
| `build` | `zup build` |
| `compose` | `zup publish stage`, folding per-target output into one release |
| `finalize` | `zup sign verify`, then reads the rewritten release description |
| `attest` | reads the release manifest and attests the final bytes it names |
| `publish` | `zup publish github` |
| `release` | build → compose → finalize → attest → publish |

`release` is a loop over the same phases a hand-written pipeline runs, not a
separate code path.

**A phase is a workflow step; an operation is what zup said it did.** The
translation is one table in `action/src/phases.ts`. Attestation is only part of
`release` when `attest: true`; `operation: attest` already implies it.

`zup ci github generate` writes the whole pipeline instead, one job per phase, so
`plan → build → compose → attest → publish` stays readable in the diff. See
[GitHub distribution](github-distribution.md).

## Installing the CLI

```text
explicit zup-path
  → requested zup-version, from the tool cache
  → download exactly that version
```

There is no fallback to a `zup` on `PATH`: a runner image's `zup` is whatever
somebody installed weeks ago, and a release that silently used it would be a
release nobody reproduced.

**The default version is the one this action was tested against**, compiled in.
A workflow pinning an action ref therefore gets reproducible tool behaviour.

An executable is never run because a URL resolved. Before anything is made
executable, the action compares the bytes against two independent records: the
SHA-256 zup recorded when it published (read from the release's own
`zup-release.json`), and the SHA-256 GitHub computed for the uploaded asset, when
the API reports one. A size mismatch, either digest mismatch, or a truncated
response refuses. The same check runs against a cache entry, because a
half-written cache directory or two concurrent jobs sharing a cache both produce
a file of the right name and the wrong bytes. `zup-path` skips all of it, which
is the point of it: a locally built zup has no published digest.

## The security model

### The token is in one subprocess

`zup build` may run Tauri, Electron, Cargo build scripts and npm scripts. A
token in that environment is a token handed to whatever the project's build does.

Every subprocess environment is **built from scratch from an allowlist** - not
"the token is scrubbed from the others" - so a variable GitHub adds next year is
not in a zup build by default. Only `zup publish github` receives it.

The token is never a CLI argument: arguments are visible in a process listing on
a shared runner. It is registered with `core.setSecret` before anything can
print it, and the action redacts every registered secret from every message it
writes.

### Dangerous triggers

`pull_request_target` and `workflow_run` execute code that may have come from a
fork, with the base repository's write token. The action **refuses** `publish`
and `release` on those events. `build`, `compose`, `setup` and `attest` are not
refused: they write nothing to the repository.

The safe pattern is a different workflow, not a flag:

```text
pull_request_target  builds the untrusted code, uploads artifacts, holds no write token
workflow_run         downloads those artifacts, publishes with a write token
```

`allow-unsafe-publish: true` emits a warning annotation even when the run
proceeds, and is refused outright when the checkout came from a fork - that is a
decision about a specific stranger's branch, not about a configuration.

| operation | permissions |
| --- | --- |
| `setup`, `build`, `compose` | none |
| `attest` | `id-token: write`, `attestations: write` |
| `publish`, `release` | `contents: write` |
| `publish --dry-run` | `contents: read` |

A dry run still needs a credential because it verifies against the live
repository.

## Diagnostics become annotations

A diagnostic arrives with a code and a source span, and becomes an annotation at
that position - relative to `project-path`, because GitHub resolves annotation
paths against the workspace root and an annotation on a path that does not
resolve is invisible.

A diagnostic arrives twice, streamed and again in the final result, and is
annotated **once**. Telling the reader the same fact twice is worse than not
telling them at all.

## A failing zup is read, not scraped

When zup exits nonzero, the action reads the failure out of the protocol
document rather than out of stderr: the stream's last line is a `completed`
event, and its result carries `status` and the diagnostic with its code and
location. Every diagnostic is annotated and the summary keeps whatever was
produced before the failure.

This is why the phases run `--format jsonl` rather than `--format json`: the
stream also gives the action something to read *while* zup works.

## Arguments

`args` is tokenized into an argument vector and never concatenated into a command
string; no process is spawned with `shell: true`, so `;`, `|`, `&&`, `$(...)` and
backticks are ordinary characters.

Quoting follows the platform, because the two families genuinely differ. On
Windows a backslash is a *path separator*: `C:\temp\dist` is one argument
containing no escapes, and treating `\` as an escape would silently delete it.
Arguments are appended last, so a typed input can be overridden.

## The toolchain split

```text
Bun  ── installs, tests, bundles ──▶  action/dist/index.js  ── runs on ──▶  Node 24
```

Three gates keep the two runtimes from blurring:

1. `tsconfig.json` - the one covering `action/src` - sets `"types": ["node"]` and
   nothing else, so a `Bun.file` or `Bun.spawn` in shipped code is a compile
   error rather than a review comment. The tests get Bun's globals through a
   separate `tsconfig.test.json`.
2. `scripts/verify-runtime.mjs` fails on any `Bun.*` that arrives from a
   transitive dependency, which type checking cannot see.
3. The same script *runs* the built artifact under Node. A bundle Bun accepts and
   Node cannot load is a green CI run and a broken release in somebody else's
   workflow.

> On Bun 1.4.2, `outfile` is accepted and **ignored**: the artifact lands at
> `./main.js` instead of the path `action.yml` names, and every step reports
> success. `scripts/build.mjs` uses `outdir` and asserts the file is where
> `action.yml` expects it.

## The bundle

`dist/index.js` is one committed file rather than a file plus lazily-loaded
chunks. A chunk graph has content-hashed names, so one missing file is an
`ERR_MODULE_NOT_FOUND` on a runner after a green build - and a content-hashed
chunk graph is a diff nobody reads. Splitting is a one-line change to
`scripts/build.mjs` plus a loop in `verify-runtime.mjs` if startup ever matters.

## Versioning and platform

The action has its own tag namespace, `action-v*`, because `v1` alone would put
two unrelated meanings on one prefix. It is one movable tag, not a version
ladder: nothing outside this repository consumes the action yet, so there is no
compatibility to promise.

`action.yml` is at the repository root and the implementation is in `action/`,
so the action and the CLI ship from one commit. Running on `node24` implies
Actions Runner 2.327.1 or newer, declared as `runs.minimum`.

## Enterprise and artifacts

Nothing about the project being built hardcodes `github.com`: workflow context
comes from `GITHUB_SERVER_URL` and `GITHUB_API_URL`, and the artifact URL is
derived from the server URL. Downloading the public zup CLI from GitHub.com is a
separate concern, is unauthenticated, and never sends the project's GHES token
there. `@actions/artifact` v4+ is unsupported on GHES; the action reports the
failure honestly.

Workflow artifacts are off by default - a release can be gigabytes. A single
already-compressed output is uploaded without a zip, using the direct artifact
support GitHub added in 2026; the service then names the artifact after the
*file*, and the log says so.

## Action dependency pins

`github-actions.lock.json` tracks what each `uses:` ref resolves to. Dependabot
advances the refs; the lock records the answer.

```bash
cargo xtask github-action-pins check             # offline; runs in CI
cargo xtask github-action-pins check --online    # also resolve against upstream
cargo xtask github-action-pins refresh           # advance series, re-record commits
```

`check` verifies lock syntax, that every recorded commit is a full lowercase
SHA, and that every `uses:` in a committed workflow is the ref the lock tracks. A
workflow bumped to `v8` in one file while the lock still says `v7` is drift.

`--online` answers the two questions a version ref cannot: is a newer major
published, and does this ref still point where it did? A moved *tag* is reported;
a moved *branch* channel is not, because a report that fires every week trains
people to ignore it.

`refresh` never runs inside a build. Resolution uses `git ls-remote`, so it needs
no token and adds no HTTP client to a tool that otherwise has none.

## Maintaining the action

```bash
cd action
bun install --frozen-lockfile
bun run check          # typecheck, lint, test, bundle, metadata, Node 24 run
bun run check:dist     # the committed bundle is current
bun run local          # run the action on this machine
```

Adding an input means adding it to `action.yml` **and** to the input table in
`action/src/inputs.ts`. `bun run metadata` fails if the two disagree in either
direction, and if the default zup version has drifted from the workspace version.

`bun test` is a compatibility gate, not just coverage: the protocol tests read
the golden fixtures from `fixtures/automation/` on disk - documents zup's own
Rust serializes - so a DTO change the decoder has not been taught fails there,
naming the field.

`real-zup` then invokes the action as `uses: ./` on `windows-latest` and
`windows-11-arm` against a zup **built from this commit**. Real zup, not a stub:
a stub that emits a hand-written document proves the action against a document
somebody typed, which is how the action and the CLI came to disagree about the
shape of a result.

## Limitations

- **A published zup release is required for the download path.** The opt-in
  `tool-bootstrap` job covers it once a release exists.
- **`operation: release` runs in one job.** A multi-job pipeline is a
  hand-written workflow or the generated one.
- **Attestation requests are strict.** A requested attestation that fails on a
  plan without attestation support is an error, because a release that looks
  attested and is not is worse than one that failed.
- **One dependency advisory is accepted.** `@actions/attest` depends on
  `@sigstore/core` ≤ 3.2.0, which has a DSSE payload-type binding advisory with no
  fixed release. A workflow that does not set `attest: true` never reaches it.
