# Secure updates

Updates use TUF metadata and static files. There is no zup update service.

An update does **not** download a complete installer. It resolves an
authenticated release graph, works out which immutable blobs this machine is
missing, and fetches only those. `docs/online-acquisition.md` is the full report;
this page is the configuration and the commands.

## Configure and build

Add `[updates]` to `zup.toml`:

```toml
schema = 1

[app]
id = "com.acme.desktop"
name = "Acme"
version = "1.4.0"
main = "Acme.exe"

[build]

[build.targets.default]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist" }

[install]
scope = "user"

[install.directory]
user = "${location.user_data}/Acme"

[updates]
repository = "https://updates.example.com/acme"
channel = "stable"
root = "update-root.json"
```

`root` is read while building and its bytes are embedded in the installer package. The built installer and the committed maintenance `Setup.exe` retain the same repository, channel, and trusted root. At runtime, the configured repository URL is the directory containing `metadata/` and `releases/`.

## Stage and publish

```powershell
zup build
zup publish stage --channel stable --output dist/web --download dist/Acme-Windows-Setup.exe
zup publish stage --thin --channel stable --output dist/web `
  --dispatcher target/release/zup-dispatch-online-i686-pc-windows-msvc.exe `
  --repository https://updates.example.com/acme
```

`zup publish stage` composes the same artifact graph `zup build` does and writes
the complete immutable web tree:

```text
dist/web/blobs/sha256/<ab>/<hex>            one compressed blob
dist/web/releases/stable.json              the release descriptor
dist/web/releases/stable/versions/1.4.0.json   the same bytes, immutably named
dist/web/releases/stable/catalog.json      digest-to-size catalog
dist/web/releases/stable/variants/*.json   one manifest per variant
dist/web/tuf-input/…                       the same documents, for tuftool
```

A generic CDN, object store, or static web server is enough. Nothing under
`blobs/` ever changes, because the name is the digest, so an origin may cache it
forever. The server understands no components, targets, or installation logic.

The same tree is a valid offline seed: `--source dist/web` satisfies an install
with no network and no second packaging format.

## Host the release on GitHub instead

A project that wants no bucket, no CDN, and no origin can put the release itself
on GitHub and let the acquisition engine read it back:

```powershell
zup publish stage --packages dist/packages
zup publish github
```

```toml
[distribution]
host = "github"

[publish.github]
repository = "acme/acme"
```

That publishes one asset per variant rather than one per content object — a
release page with nine thousand hexadecimal filenames is unusable, and GitHub's
per-release asset limit is a thousand — and the client reads it back as ordinary
verified content, indistinguishable from a CDN's.

The credentials, the discovery rules, the state machine, the generated release
workflow, the range behaviour, and the honest list of what this is not are all in
[GitHub distribution](github-distribution.md).

## The two thin installers

`--thin` emits two files, and they are one artifact with two promises:

```text
Acme-Setup-version.exe   reads releases/stable/versions/1.4.0.json
Acme-Setup-channel.exe   reads releases/stable.json
```

A version-labelled installer always installs the release it was built for, because
it reads an immutable name that nothing rewrites. A channel installer installs
whatever the channel currently says. Everything else about them is identical.

`--dispatcher` is the launcher the thin installers are built from, and it must be
the **online** dispatcher. A thin installer is that launcher plus an index and a
trust block - a few kilobytes over 3.87 MiB, for a release of any size. A
dispatcher built without the `online` feature refuses a thin artifact with a clear
reason rather than pretending, so the mistake is visible immediately.

`--repository` is the URL a published client reads, and it is embedded. It has to
be the address clients will use, not the path this build happens to write to; it
defaults to the staged tree as a `file:` URL, which is right for a local origin and
obviously wrong for a real one. `--thin-output` chooses where the two installers
are written, defaulting to the directory beside the web tree, because the tree is
what a static origin serves and an installer is what a person downloads.

`--channel` must match the manifest's `[updates] channel`. A build that let them
differ would publish a launcher pointing at a document nobody signed, so it is
refused.

**The trusted root is inlined**, not shipped beside the installer. It is read and
validated when the project is materialized, so a publisher cannot ship a launcher
with an unparseable root without finding out at build time.

**A thin release's runtime is not the template.** It is the template with this
target's plan compiled into it and none of the content - a few megabytes, and it
knows exactly what it would install. That image is staged into the web tree as
ordinary verified content, because a bootstrapper that fetched something the graph
did not name would have nothing to check it against.

## Sign with tuftool

Keep signing keys outside zup. `tuftool` is the standard repository creation and
update tool from the `awslabs/tough` project, and it already knows how to sign a
TUF repository. `zup publish stage` writes the documents into `tuf-input` in
exactly the shape `--add-targets` reads:

```powershell
tuftool create --root $trustedRoot --key $signingKey --add-targets dist/web/tuf-input `
  --targets-expires 'in 3 weeks' --targets-version 1 `
  --snapshot-expires 'in 3 weeks' --snapshot-version 1 `
  --timestamp-expires 'in 1 week' --timestamp-version 1 `
  --outdir dist/repository
```

Publish the contents of `dist/repository/metadata/` and `dist/web/` together to
static HTTP storage. For subsequent releases, use `tuftool update` and
monotonically increase targets, snapshot, and timestamp versions.

Configure TUF role thresholds and keys in `root.json` for the deployment; keep
root and targets signing offline and use a restricted online timestamp key for
timestamp refreshes. Root rotation is published through the normal versioned TUF
root chain. Ship the new root out-of-band in newly built installers; existing
clients follow and verify repository root rotations from their embedded trusted
root.

For smoke checks, `tuftool download --root <trusted-root> --metadata-url <repo>/metadata --targets-url <repo>/targets <output-dir>` exercises the same standard static layout. The update client also has local filesystem repository integration tests using `tough`'s repository editor and verifier.

## What the client does

`Setup.exe update check` reads the signed channel target and reports the current
release. `Setup.exe update` resolves the release graph, selects the variant this
machine already runs, compares the required digests against its verified cache,
downloads only the missing blobs, and runs the ordinary upgrade lifecycle against
them. It does not download a new `Setup.exe`.

Both accept `--scope`, `--state-root`, `--output`, `--non-interactive`, and
`--yes`; `--scope` takes `user`, `machine`, or `either`, and `--output` takes
`human`, `json`, or `jsonl`. `--source` points at a local directory that satisfies
the content side of the closure, and it is verified exactly like anything off the
network: a seed is a place to fetch from, never a place to trust.

Safe TUF expiration checks are always enabled. Timestamp, snapshot, and targets
metadata are persisted at
`<update-state-root>/updates/<app-id>/<channel>/tuf/`; do not delete that
directory to recover from a verification failure. The datastore is namespaced by
the trusted root's digest, the repository's fingerprint, and the channel, so two
repositories - or two channels - cannot poison each other's rollback memory, and
trusted metadata is never deleted on failure. Root rotation is followed from the
embedded root through the normal versioned chain.

For machine installs, the invoking user's `%LOCALAPPDATA%\zup` is the update
state root so metadata and the verified content are writable before the existing
lifecycle requests elevation for machine changes.

Target bytes remain in a private `.partial` file in the per-user content cache
until the complete stream hashes to the digest the release authenticated, and are
then published by an atomic rename. The downloaded installer publishes itself as
maintenance only if its transaction commits.

A downgrade is refused: the lifecycle requires the new package version to be
greater than the installed version, an equal version is a modify, and a lower
version is an error. There is no manifest option to allow it.

## Install, update, repair, and modify are one graph

These are the same call with a different closure, and there is one implementation
of it (`crates/zup/src/graph.rs`):

| Operation | Closure |
| --- | --- |
| install, update | the whole selection |
| `--enable` / `--disable` | the newly-enabled components' content |
| repair | the digests the ledger says drifted |

A repair's closure is arithmetic, not a policy: the ledger holds each owned
resource's digest, so "drifted" means an owned file with no bytes on disk, or bytes
that hash to something else. The closure is the intersection of that set with what
the release carries - which doubles as the ownership check, because a digest in
neither cannot be asked for. **A one-file repair costs one file.**

A repair with no authenticated source is refused with an explicit *repair source
unavailable*. It is not a weakened ownership or integrity check: the ledger is the
authority on what the machine owns, and nothing about a missing file relaxes it.
Every refusal on this path leaves the machine unchanged, and that is asserted
rather than assumed.

## A thin runtime runs its own operations

A runtime installed from a graph holds a **plan-only** package: its plan and none
of its content, because the content came from - and comes again from - the release
graph. That package is the runtime's whole identity, and it is where the
`[updates]` configuration travels, so a machine that has lost its installer file
can still repair itself.

So `Setup.exe repair`, `Setup.exe modify`, and a second `Setup.exe install` on a
graph installation all take the same path the bootstrapper took, and the embedded
plan is checked against the graph before anything is touched. If the installation
in a given scope belongs to a different application, or has no release identity
at all, the operation says so rather than fetching a different release's bytes.

An offline installer still has its own embedded package and its own lifecycle; the
two paths share the engine, the ledger, and the transaction rules, and nothing
else.
