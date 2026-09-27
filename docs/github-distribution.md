# GitHub Releases as a distribution host

A project that wants a thin installer to fetch its content, and wants to run no
server, gets this:

```bash
zup publish stage --packages dist/packages
zup publish github
```

```text
Acme-Windows-Setup.exe        a person downloads this
zup-release.json              the release description
Acme-Windows-x64.zup          one variant's content, one asset
Acme-Windows-arm64.zup        the other variant's content, one asset
zup-package-win-x64.json      the small document that names the package
zup-package-win-arm64.json
zup-release-stable.json       the authenticated release descriptor
zup-catalog-stable.json       the authenticated content catalog
zup-tuf-1.root.json           the trust anchor, when the project signs
```

A release page that reads like a release page, and content a machine can use. No
bucket, no CDN, no origin, no upload tooling.

## The four planes

GitHub is a *provider* here, not an architectural dependency. Four planes, three
crates, and the dependency only runs one way:

```text
zup-publish              ReleasePlan, HostLimits, classify(), naming, receipts
       │
       └─ zup-publish-github      the release plane's GitHub implementation
              │
              └─ zup-distribute-github   the content plane's GitHub implementation
```

| plane | crate | what it knows |
| --- | --- | --- |
| build | `zup-build`, `zup-artifact` | what a release contains |
| release | `zup-publish`, `zup-publish-github` | how files become a release on a host |
| content | `zup-distribute-github`, `zup-acquire` | how a client reads that release back |
| runtime trust | `zup-runtime`, `zup-acquire` | what a client believes, and why |

`zup-core`, `zup-plan`, `zup-runtime`, `zup-transaction`, and artifact semantics
contain no GitHub concept at all. Neither does `zup-publish`: it has a
`ReleasePlan`, host limits, and an asset classifier, and it could be pointed at
GitLab, SourceForge, or an S3 bucket with no change to anything above it. There is
no `if tauri` or `if electron` in the GitHub publisher, and there is no `if
github` in the acquisition engine.

`zup-publish-github` depends on `zup-manifest` for configuration types. The
manifest knows no provider; the provider reads the manifest.

## The release plan

Everything a release contains is one `ReleasePlan`, and the plan separates roles
rather than listing files:

| role | what it is | class |
| --- | --- | --- |
| `Install` | a file a person downloads to install | user-facing |
| `Update` | a file an updater fetches to become a later version | either |
| `Auxiliary` | update manifests, signatures, blockmaps, checksums | transport |
| `Manifest` | machine-readable descriptions of the release | either |

A file may hold two roles — an install artifact and an update artifact are often
the same `.exe` — and it is uploaded once. A name in two roles with *different*
bytes is refused at plan validation rather than resolved by a race.

`ProductClass` is the distinction that decides refusal from sharding:

- **user-facing** over the host's per-asset limit is a **refusal**. A `Setup.exe`
  that arrives as pieces is not the installer that was signed.
- **transport** over the limit is a **sharding decision**, because a transport
  package is an internal object the acquisition engine reads and no person ever
  sees it split.

## `zup publish github`

```text
--manifest <MANIFEST>      [default: zup.toml]
--release-dir <DIR>        what a build wrote          [default: dist]
--web <DIR>                the staged content tree
--packages <DIR>           the transport packages
--repo <OWNER/NAME>        or host/owner/name for Enterprise
--tag <TAG>                [default: v<version>]
--draft
--prerelease
--dry-run                  plan and verify everything, write nothing
--replace-conflicts        replace a differing asset, drafts only
--notes-text <TEXT>
--receipt <PATH>
--format <human|json>
```

Everything is derived. The repository comes from `--repo`, then
`[publish.github] repository`, then `GITHUB_REPOSITORY`, then a GitHub remote —
and when none of them names one, the command **refuses** rather than guessing.
The tag is derived from the version. The asset list comes from
`zup-release.json` and the staged tree. The digests come from the files.

Discovery prefers `origin`, then `push`, then `fetch`, and reduces every common
remote form (`git@host:owner/name.git`, `https://host/owner/name`,
`ssh://git@host/owner/name`) to the same three strings. A non-GitHub remote is not
a candidate. Several GitHub remotes and no `origin` is an actionable refusal.
An Enterprise hostname is preserved, so `git.acme.internal:acme/acme` stays on that
installation.

### Credentials

`GH_TOKEN`, then `GITHUB_TOKEN`, then `gh auth token`. Never in `zup.toml` - the
schema has no field for one, so a project that tries is stopped at the parse
rather than at a later "you should not do that". Never printed, never logged,
never in a receipt. The type that holds a token stores it in a
`secrecy::SecretString`: `Debug` prints `[REDACTED]`, `Display` is not
implemented, and the value is only reachable through an `expose_secret` call that
reads like the dangerous thing it is.

That last point is why the dependency exists rather than a hand-written `Debug`
impl. A hand-written impl is correct until somebody derives `Debug` on a struct
that holds a token, at which point it prints the value and nothing in the type
system objects. `SecretString` cannot be derived into anything that reveals it.
The `Authorization` header is still built by hand from `expose()` and is still
marked sensitive on the wire - redacting a `Debug` impl does nothing about a
proxy log.

A dry run in a workflow that holds a job-scoped token gets that token in exactly
one process's environment. `docs/action.md` covers how the GitHub action scopes
it, which matters because a build may run arbitrary project build scripts.

A dry run with no credential prints the plan and says publication was not
attempted. That is deliberate: "would this work" includes "could this
authenticate", and a dry run that cannot answer that should say so rather than
fail.

### What a publication does

```text
resolve and verify the tag
    ↓
create or find a draft          resumable
    ↓
upload every required asset     resumable; a matching asset is skipped
    ↓
verify every remote asset       name, size, sha256 digest, state
    ↓
publish the draft, once
```

The draft is the point. A failed upload leaves a draft with eleven of twelve
files on it, which is a state a rerun finishes rather than a state to throw away.
The release becomes public in exactly one call, after every file has been proved
against the digest the local build computed.

Idempotence, precisely:

- a matching asset is **skipped**
- a differing asset on a draft is a **failure**; `--replace-conflicts` replaces
  it, and only on a draft
- a published release is **never mutated** — a non-`Present` action on one fails
  even when the name was never there
- an exactly matching published release is a **successful no-op**

An upload that fails ambiguously can leave GitHub's `starter` asset behind.
Reconciliation is explicit: the publisher looks for that state, deletes it, and
retries with bounded backoff, honouring `Retry-After` and the rate-limit headers.
Asset uploads are deliberately **not** retried inside the HTTP client —
reconciliation has to happen *between* attempts, and only the publisher can do it.

### Release notes

GitHub generates the body by default. An authored body is never overwritten: a
project that keeps notes in a file gets the file's text plus a generated download
table, and a draft Release Drafter has made is left alone. `[publish.github.notes]`
is `generated`, `file`, `text`, or `none`.

### Limits, and what is refused before anything is written

| limit | value | what happens past it |
| --- | --- | --- |
| assets per release | 1000 | the API refuses the upload; the release is unfixable |
| bytes per asset | 2 GiB | the API refuses the upload |
| total release size | none | a 400 GiB release is allowed and is nobody's problem |
| bandwidth quota | none | throughput is throttled, not refused |

`zup publish github --dry-run` runs the whole preflight and mutates nothing. A
release that cannot be published is refused before a draft exists, rather than
after eleven gigabytes have been uploaded to one.

A transport package is packed to a 1536 MiB ceiling rather than 2 GiB, because a
package exactly at the limit has no room for a host that rounds or appends a
trailer.

### Receipts

The provider's receipt is separate from `zup-release.json` and holds GitHub's own
identifiers — release id, url, asset ids, upload state. It holds no secret and no
content digest zup does not already hold, and it is the thing a maintainer reads
when a release needs to be found again.

### Immutability

GitHub's immutable releases are **designed for**, and the provider reports what
the host says. `immutable_releases` is `None` on a server too old to have the
field, and "not reported" is never rendered as "not enabled". zup does not enable
the setting for a project; the answer is Settings → Releases, because a provider
silently changing a repository's settings is a worse surprise than a documented
click. What zup guarantees instead is that it never mutates a published release:
once public, those bytes are what somebody downloaded, and the publisher refuses
any action on them.

## Reading it back

```toml
[distribution]
host = "github"

[publish.github]
repository = "acme/acme"
```

### Packages, not blob paths

A release should read like a release page. A project with nine thousand content
objects must not turn that into nine thousand assets — and must not, because the
per-release limit is a thousand and a repository whose graph has become a list of
hexadecimal filenames has stopped being a graph.

So content travels as **one package per variant**:

```text
offset 0   "ZUPGPKG\0"  8 bytes magic
offset 8   u32 LE       schema
offset 12  u64 LE       required feature bits
offset 20  u64 LE       metadata length
offset 28  [u8; 32]     SHA-256 of the metadata bytes
offset 60  metadata     JSON, SHA-256 protected
           frames       one Zstandard frame per blob, ascending by digest
```

A frame's digest is the digest of the *uncompressed* blob, byte for byte the
digest the release's content catalog names. Packing a hundred blobs into one file
changes how they are transported and nothing about what they are — which is what
makes moving a project from GitHub-only distribution to a real CDN a configuration
change rather than a content migration.

The header is a header because sharding needs somewhere to say where the shards
are, and the first shard is where the answer belongs. A sharded package's
descriptor is readable from shard 0 alone, which is what lets a client open a
package by fetching one small file rather than a directory listing. Piece 0
therefore begins at byte 0 and holds the header, the index, and whatever frames
fit, so the pieces tile the whole package with no gap.

### A locator, not a trust anchor

`zup-package-win-x64.json` says *where* the bytes are: the package's name, its
digest, its length, and its pieces. It does **not** decide what is installed.
Every blob inside the package is verified against a digest from the release's own
authenticated content catalog before it is published into the cache, so a
tampered descriptor can cause a wrong-blob attempt and a wasted download — both
caught — and cannot cause unverified content to be installed.

The descriptor is still worth publishing, for one reason: when a project *does*
sign its release through TUF, it is one more small named thing to put in the
signed targets, and sharding cannot work without somewhere authenticated to
record the shard map.

### Two release references

```text
Pinned   https://github.com/owner/repo/releases/download/v1.4.0/<asset>
Latest   https://github.com/owner/repo/releases/latest/download/<asset>
```

A pinned reference is an identity: it names one release and resolves to the same
bytes forever. A version-pinned thin installer must use it, because a bootstrapper
that can silently become a later release is a bootstrapper that will.

`latest` is a *channel*, and it is the one place zup uses GitHub's own opinion
about which release is newest. It cannot express `beta`, `nightly`, or `canary`,
and zup does not pretend otherwise. Complex channels stay on the generic TUF path,
where a channel is a signed pointer rather than a redirect.

The redirect is transport, never trust. A signed URL with an expiry in it is a
fetch target for one transfer; caching it as an address would make a temporary
credential into a permanent identity.

### Range support is measured, not assumed

GitHub does not document HTTP Range support for release assets. It may work; it
may work today and not next year; and a client that depends on it produces a
broken install for some users on the day it stops. So range is probed:

| the host answers | what zup does |
| --- | --- |
| `206` with a `Content-Range` that lines up | uses it, and records that the host does ranges |
| `200` | reads the frame out of the whole piece — correct, slower, never a failure |
| `206` with a `Content-Range` that does not line up | **refuses the range** and reads the whole piece |

The third row is the interesting one. A host that disagrees about what byte *n* is
is a host whose bytes cannot be trusted at all, and reading the wrong bytes is
worse than reading all of them. Once that has been seen, the host is not asked
again: a misaligned range is a property of the host, not of one unlucky frame.

All three outcomes settle the question for the rest of the source's life, and
`Metrics` counts which happened, so `zup doctor` can say whether a project is
actually range-accelerated or quietly downloading whole packages.

### Resume

A frame is streamed into the cache chunk by chunk, at the cache's resume offset.
Reading a response whole first would mean a connection that drops at ninety
percent of a ninety-megabyte blob costs all ninety megabytes, because nothing
reached disk. So an interrupted transfer leaves a partial, and the next attempt
asks for exactly the bytes the cache does not have.

## `zup ci github`

```bash
zup ci github generate      write .github/workflows/release.yml
zup ci github check         say whether the committed workflow is current
```

A committed file, and a readable pipeline. Both read the same manifest and
produce the same bytes, which is what makes `check` meaningful: it can differ
from the file on disk only when the generator or the manifest changed, which is a
change somebody made on purpose. `generate` will not overwrite a file that
differs without `--force`, because a generated workflow nobody reviews is just a
workflow nobody read.

The generated pipeline has explicit phases:

```text
plan  →  build (matrix)  →  compose  →  attest  →  publish
```

- the matrix comes from the manifest's own target profiles, so nobody enumerates
  targets in YAML and keeps the two in sync
- runners are the current native labels, including `windows-11-arm`; a target with
  no native runner is marked cross-compiled rather than given a lie
- each phase calls the official zup action, which installs a released zup rather
  than compiling one. That is not brevity: `cargo build -p zup` only ever worked
  inside the zup repository, so the generated file was correct for exactly one
  project
- every *third-party* action uses the ref `github-actions.lock.json` tracks, and
  `check` fails a committed workflow that drifts off it
- the publish credential is an action input to one step, not a step-level `env:`
  on several. `zup build` may run Tauri, Electron, Cargo build scripts and npm
  scripts, and a token in that environment is a token handed to whatever the
  project's build does
- attestation uses `actions/attest`. `actions/attest-build-provenance` is now only
  a wrapper on top of it
- permissions are least-privilege at the top and narrower per job: only `publish`
  can write to the repository
- `cancel-in-progress: false`, because cancelling a half-published release is
  worse than waiting
- a release environment is opt-in, because required reviewers and environment
  secrets are three clicks on a setting and reimplementing them in zup would be a
  worse version of the same thing

The action ref is a floating major by default and a project can name an exact one
under `[publish.github.workflow] action`. The generated file always shows exactly
what it will run.

`check --format json` reports the workflow path, whether it is current, the
derived tag, every target with its runner and whether it is native, the
attestation and signing settings, and every pinned action with its SHA and the
date it was last resolved. Actions a generated workflow does not use — zup's own
CI dependencies — are marked as such, so a project is not told to pin a Rust cache
its pipeline does not have. That is enough for CI to fail on drift without parsing
prose.

The generated workflow is **optional**. `docs/action.md` covers the other way to
release: writing the phases yourself with repeated invocations of the action. The
two are equally valid, and which one a project wants is a decision about how much
of the pipeline it wants to own.

## Enterprise

One hostname, three bases, three different services:

| base | github.com | Enterprise |
| --- | --- | --- |
| web | `https://github.com/` | `https://<host>/` |
| api | `https://api.github.com/` | `https://<host>/api/v3/` |
| upload | `https://uploads.github.com/` | `https://<host>/uploads/` |

Uploads are a separate service with a separate rate limit, which is why an
implementation that shares one base with the API cannot read the upload budget
off an API response. `GITHUB_API_URL` and `GITHUB_SERVER_URL` are honoured when
set. Features a given server version may not have — immutable releases above all —
are feature-detected and reported as "not reported".

## What this is deliberately not

- **Not a case per content object.** One or two assets per variant, always.
- **Not a second trust mechanism.** The content catalog decides; the package
  descriptor only says where the bytes are.
- **Not a channel system.** One stable channel, and it is GitHub's.
- **Not a cross-variant deduplicating store.** Two variants sharing content still
  publish both copies, because each package is a separate asset on a host with no
  cross-asset storage. What deduplicates is the *asset count*, and that is the
  thing that breaks.
- **Not a replacement for a CDN.** See below.

## Limitations

- **A host without range support costs one whole piece per blob.** Correct, and
  potentially an order of magnitude more than the content. `Metrics` reports
  `ranged_transfers` and `fallback_transfers` so a project finds out before its
  users do; a project whose host does not serve ranges should use a CDN.
- **Bytes do not deduplicate across variants.** A five-architecture release
  publishes roughly five times the shared content. Above roughly 1.5 GiB per
  variant this becomes the reason to move to a real content origin — the packages
  are already portable, so it is a configuration change.
- **A private repository cannot serve a thin installer.** Release assets need a
  credential and a bootstrapper cannot carry one. `zup doctor` reports it.
- **Immutable releases are recommended, not enforced.** The setting is the
  repository owner's to turn on.
- **The live harness is read-only.** `zup-publish-github/tests/live.rs` probes a
  real repository and a real asset but creates, uploads, publishes, and deletes
  nothing:

  ```bash
  ZUP_GITHUB_LIVE=1 ZUP_GITHUB_LIVE_REPO=owner/name GH_TOKEN=… \
    cargo nextest run -p zup-publish-github --test live
  ```

  Set `ZUP_GITHUB_LIVE_ASSET` to an asset name to have it report whether that
  host serves byte ranges.

## Testing

| suite | what it covers |
| --- | --- |
| `zup-publish-github/tests/publisher.rs` | 52 scenarios against a mock GitHub: the state machine, discovery, endpoints, credentials, notes, workflow generation |
| `zup-distribute-github/tests/distribution.rs` | pinned and `latest` discovery, the right variant only, package-to-cache import, corrupt frames, cache reuse, sharding, missing shards, the preflight, and all three range answers |
| `zup-distribute-github/tests/measurements.rs` | asset count, bytes cold and warm, request count, ranged against whole, and what a resume saves |
| `zup/tests/github_publish.rs` | the two commands against the real binary: reproducibility, a stale workflow, a dry run that publishes nothing, and a manifest with nowhere to put a token |
| `zup-publish-github/tests/live.rs` | opt-in, read-only, against GitHub itself |
