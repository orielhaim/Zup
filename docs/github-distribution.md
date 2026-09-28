# GitHub Releases as a distribution host

A project that wants a thin installer to fetch its content, and wants to run no
server:

```bash
zup publish stage --packages dist/packages
zup publish github
```

```text
Acme-Windows-Setup.exe        a person downloads this
zup-release.json              the release description
Acme-Windows-x64.zup          one variant's content, one asset
zup-catalog-stable.json       the authenticated content catalog
zup-tuf-1.root.json           the trust anchor, when the project signs
```

## Four planes

GitHub is a *provider* here, not an architectural dependency. The dependency runs
one way:

```text
zup-publish              ReleasePlan, HostLimits, classify(), naming, receipts
        │
        └─ zup-publish-github      the release plane's GitHub implementation
               │
               └─ zup-distribute-github   the content plane's GitHub implementation
```

| plane | crates | what it knows |
| --- | --- | --- |
| build | `zup-build`, `zup-artifact` | what a release contains |
| release | `zup-publish`, `zup-publish-github` | how files become a release on a host |
| content | `zup-distribute-github`, `zup-acquire` | how a client reads that release back |
| runtime trust | `zup-runtime`, `zup-acquire` | what a client believes, and why |

`zup-core`, `zup-plan`, `zup-runtime`, `zup-transaction`, artifact semantics, and
`zup-publish` itself contain no GitHub concept at all. `zup-publish` has a
`ReleasePlan`, host limits, and an asset classifier; it could be pointed at GitLab
or S3 with no change above it. There is no `if tauri` in the publisher and no
`if github` in the acquisition engine.

## The release plan

Everything a release contains is one `ReleasePlan`, which separates roles rather
than listing files:

| role | what it is | class |
| --- | --- | --- |
| `Install` | a file a person downloads to install | user-facing |
| `Update` | a file an updater fetches to become a later version | either |
| `Auxiliary` | update manifests, signatures, blockmaps, checksums | transport |
| `Manifest` | machine-readable descriptions of the release | either |

A file may hold two roles - an install artifact and an update artifact are often
the same `.exe` - and is uploaded once. A name in two roles with *different* bytes
is refused at plan validation rather than resolved by a race.

`ProductClass` decides refusal from sharding: **user-facing** over the host's
per-asset limit is a refusal, because an installer that arrives as pieces is not
the installer that was signed. **transport** over the limit is a sharding
decision, because nobody ever sees it split.

## `zup publish github`

Everything is derived. The repository comes from `--repo`, then
`[publish.github] repository`, then `GITHUB_REPOSITORY`, then a GitHub remote -
and when none of them names one, the command **refuses** rather than guessing.
The tag is derived from the version, the asset list from `zup-release.json`, the
digests from the files.

Discovery prefers `origin`, then `push`, then `fetch`, and reduces every common
remote form to the same three strings. A non-GitHub remote is not a candidate;
several GitHub remotes and no `origin` is an actionable refusal. An Enterprise
hostname is preserved.

### Credentials

`GH_TOKEN`, then `GITHUB_TOKEN`, then `gh auth token`. Never in `zup.toml` - the
schema has no field for one, so a project that tries is stopped at the parse.
Never printed, logged, or written to a receipt.

The type that holds a token stores it in a `secrecy::SecretString`: `Debug`
prints `[REDACTED]`, `Display` is not implemented, and the value is reachable only
through an `expose_secret` call that reads like the dangerous thing it is. A
hand-written `Debug` would be correct until somebody derives `Debug` on a struct
holding a token, at which point it prints the value and nothing in the type system
objects. The `Authorization` header is still built by hand from `expose()` and
still marked sensitive on the wire, because redacting a `Debug` impl does nothing
about a proxy log.

A dry run with no credential prints the plan and says publication was not
attempted: "would this work" includes "could this authenticate". See
[action.md](action.md) for how the action scopes the token, which matters because
a build may run arbitrary project build scripts.

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

The draft is the point: a failed upload leaves a draft with eleven of twelve files
on it, which is a state a rerun finishes rather than a state to throw away. The
release becomes public in exactly one call, after every file has been proved
against the digest the local build computed.

Idempotence, precisely:

- a matching asset is **skipped**;
- a differing asset on a draft is a **failure**; `--replace-conflicts` replaces it,
  and only on a draft;
- a published release is **never mutated** - a non-`Present` action on one fails
  even when the name was never there;
- an exactly matching published release is a **successful no-op**.

An upload that fails ambiguously can leave GitHub's `starter` asset behind.
Reconciliation is explicit: the publisher looks for that state, deletes it, and
retries with bounded backoff, honouring `Retry-After` and the rate-limit headers.
Asset uploads are deliberately **not** retried inside the HTTP client -
reconciliation has to happen *between* attempts, and only the publisher can do it.

### Limits, and what is refused before anything is written

| limit | value | past it |
| --- | --- | --- |
| assets per release | 1000 | the API refuses; the release is unfixable |
| bytes per asset | 2 GiB | the API refuses |
| total release size | none | a 400 GiB release is nobody's problem |

`--dry-run` runs the whole preflight and mutates nothing, so an unpublishable
release is refused before a draft exists rather than after eleven gigabytes have
been uploaded to one. A transport package is packed to a 1536 MiB ceiling rather
than 2 GiB, because a package exactly at the limit has no room for a host that
rounds or appends a trailer.

### Notes and receipts

GitHub generates the body by default. An authored body is never overwritten: a
project that keeps notes in a file gets the file's text plus a generated download
table, and a draft Release Drafter has made is left alone.
`[publish.github.notes]` is `generated`, `file`, `text`, or `none`.

The provider's receipt is separate from `zup-release.json` and holds GitHub's own
identifiers - release id, url, asset ids, upload state. No secret, no content
digest zup does not already hold.

### Immutability

GitHub's immutable releases are supported, and the provider reports what the host
says. `immutable_releases` is `None` on a server too old to have the field, and
"not reported" is never rendered as "not enabled". zup does not enable the setting
for a project - the answer is Settings → Releases - and guarantees instead that it
never mutates a published release, because once public those bytes are what
somebody downloaded.

## Reading it back

```toml
[distribution]
host = "github"

[publish.github]
repository = "acme/acme"
```

### Packages, not blob paths

A project with nine thousand content objects must not turn that into nine thousand
assets - the per-release limit is a thousand, and a repository whose graph has
become a list of hexadecimal filenames has stopped being a graph. So content
travels as **one package per variant**:

```text
offset 0   "ZUPGPKG\0"  8 bytes magic
offset 8   u32 LE       schema
offset 12  u64 LE       required feature bits
offset 20  u64 LE       metadata length
offset 28  [u8; 32]     SHA-256 of the metadata bytes
offset 60  metadata     JSON, SHA-256 protected
           frames       one Zstandard frame per blob, ascending by digest
```

A frame's digest is the digest of the *uncompressed* blob, byte for byte the digest
the release's catalog names. Packing a hundred blobs into one file changes how
they are transported and nothing about what they are, which is what makes moving
from GitHub-only distribution to a real CDN a configuration change rather than a
content migration. The header exists because sharding needs somewhere to record
where the shards are, and it means a sharded package's descriptor is readable from
shard 0 alone.

### A locator, not a trust anchor

`zup-package-win-x64.json` says *where* the bytes are: the package's name,
digest, length, and pieces. It does **not** decide what is installed. Every blob
inside is verified against a digest from the release's own authenticated catalog
before it is published into the cache, so a tampered descriptor can cause a
wrong-blob attempt and a wasted download - both caught - and cannot cause
unverified content to be installed. It is still worth publishing because sharding
cannot work without somewhere authenticated to record the shard map.

### Two release references

```text
Pinned   https://github.com/owner/repo/releases/download/v1.4.0/<asset>
Latest   https://github.com/owner/repo/releases/latest/download/<asset>
```

A pinned reference is an identity: it names one release and resolves to the same
bytes forever. A version-pinned thin installer must use it. `latest` is a
*channel*, and it is the one place zup uses GitHub's own opinion about which
release is newest; it cannot express `beta` or `nightly`, and complex channels stay
on the generic TUF path where a channel is a signed pointer rather than a
redirect. The redirect is transport, never trust: caching a signed URL as an
address would make a temporary credential into a permanent identity.

### Range support is measured, not assumed

GitHub does not document HTTP Range support for release assets, and a client that
depends on undocumented behaviour produces a broken install for some users on the
day it stops. So range is probed:

| the host answers | what zup does |
| --- | --- |
| `206` with a `Content-Range` that lines up | uses it, and records that the host does ranges |
| `200` | reads the frame out of the whole piece - correct, slower, never a failure |
| `206` with a `Content-Range` that does not line up | **refuses the range** and reads the whole piece |

The third row is the important one. A host that disagrees about what byte *n* is a
host whose bytes cannot be trusted at all. Once that has been seen the host is not
asked again, and `Metrics` counts which of the three outcomes happened.

A frame is streamed into the cache chunk by chunk at the cache's resume offset, so
an interrupted transfer leaves a partial and the next attempt asks for exactly the
bytes the cache does not have. Reading a response whole first would mean a
connection that drops at ninety percent of a ninety-megabyte blob costs all ninety
megabytes, because nothing reached disk.

## `zup ci github`

```bash
zup ci github generate      write .github/workflows/release.yml
zup ci github check         say whether the committed workflow is current
```

Both read the same manifest and produce the same bytes, which is what makes
`check` meaningful: it can differ from the file on disk only when the generator or
the manifest changed. `generate` will not overwrite a differing file without
`--force`, because a generated workflow nobody reviews is a workflow nobody read.

The generated pipeline has explicit phases - `plan → build (matrix) → compose →
attest → publish` - where the matrix comes from the manifest's own profiles so
nobody enumerates targets in YAML and keeps the two in sync. A target with no
native runner is marked cross-compiled rather than given a lie. Every
*third-party* action uses the ref `github-actions.lock.json` tracks. Permissions
are least-privilege at the top and narrower per job. `cancel-in-progress: false`,
because cancelling a half-published release is worse than waiting. The publish
credential is an action input to one step, not a step-level `env:` on several.

`check --format json` reports the workflow path, whether it is current, the
derived tag, every target with its runner and whether it is native, the
attestation and signing settings, and every pinned action with its SHA and the
date it was last resolved.

The generated workflow is **optional**. The hand-written form is equally valid,
and which one a project wants is a decision about how much of the pipeline it wants
to own. See [action.md](action.md).

## Enterprise

One hostname, three bases, three different services:

| base | github.com | Enterprise |
| --- | --- | --- |
| web | `https://github.com/` | `https://<host>/` |
| api | `https://api.github.com/` | `https://<host>/api/v3/` |
| upload | `https://uploads.github.com/` | `https://<host>/uploads/` |

Uploads are a separate service with a separate rate limit, which is why an
implementation sharing one base with the API cannot read the upload budget off an
API response. `GITHUB_API_URL` and `GITHUB_SERVER_URL` are honoured when set, and
features a given server version may not have - immutable releases above all - are
feature-detected and reported as "not reported".

## What this is deliberately not

- **Not a case per content object.** One or two assets per variant, always.
- **Not a second trust mechanism.** The content catalog decides; the package
  descriptor only says where the bytes are.
- **Not a channel system.** One stable channel, and it is GitHub's.
- **Not a cross-variant deduplicating store.** Two variants sharing content still
  publish both copies, because each package is a separate asset. What
  deduplicates is the *asset count*, and that is the thing that breaks.
- **Not a replacement for a CDN.**

## Limitations

- **A host without range support costs one whole piece per blob.** Correct, and
  potentially an order of magnitude more than the content. `Metrics` reports
  `ranged_transfers` and `fallback_transfers` so a project finds out before its
  users do.
- **Bytes do not deduplicate across variants.** A five-architecture release
  publishes roughly five times the shared content. Above roughly 1.5 GiB per
  variant this is the reason to move to a real content origin - the packages are
  already portable, so it is a configuration change.
- **A private repository cannot serve a thin installer.** Release assets need a
  credential and a bootstrapper cannot carry one. `zup doctor` reports it.
- **Immutable releases are recommended, not enforced.** The setting is the
  repository owner's to turn on.
- **The live harness is read-only.** `zup-publish-github/tests/live.rs` probes a
  real repository and asset but creates, uploads, publishes, and deletes nothing:

  ```bash
  ZUP_GITHUB_LIVE=1 ZUP_GITHUB_LIVE_REPO=owner/name GH_TOKEN=… \
    cargo nextest run -p zup-publish-github --test live
  ```
