# Secure updates

Updates use TUF metadata and static files. There is no zup update service.

An update does **not** download a complete installer. It resolves an
authenticated release graph, works out which immutable blobs this machine is
missing, and fetches only those. [Online acquisition](online-acquisition.md) is
the full report; this page is the configuration and the commands.

## Configure

```toml
[updates]
repository = "https://updates.example.com/acme"
channel = "stable"
root = "update-root.json"
```

`root` is read while building and its bytes are embedded in the installer package.
The built installer and the persisted `maintenance.exe` retain the same
repository, channel, and trusted root, so a machine that lost its installer file
can still repair itself. At runtime the repository URL is the directory containing
`metadata/` and `releases/`.

## Stage and publish

```powershell
zup build
zup publish stage --channel stable --output dist/web --download dist/Acme-Windows-Setup.exe
zup publish stage --thin --channel stable --output dist/web `
  --repository https://updates.example.com/acme
```

`zup publish stage` composes the same artifact graph `zup build` does and writes
the complete immutable web tree:

```text
dist/web/blobs/sha256/<ab>/<hex>                 one compressed blob
dist/web/releases/stable.json                   the release descriptor
dist/web/releases/stable/versions/1.4.0.json    the same bytes, immutably named
dist/web/releases/stable/catalog.json           digest-to-size catalog
dist/web/releases/stable/variants/*.json        one manifest per variant
dist/web/tuf-input/…                            the same documents, for tuftool
```

A generic CDN, object store, or static web server is enough. Nothing under
`blobs/` ever changes, because the name is the digest. The same tree is a valid
offline seed: `--source dist/web` satisfies an install with no network and no
second packaging format.

The launcher is not named on the command line: `cargo xtask toolchain build`
stages it beside `zup` and the resolver finds the online flavour there.
`--dispatcher` is the escape hatch for a path outside the staged toolchain, and it
must be the **online** launcher - a launcher built without the `online` feature
refuses a thin artifact with a clear reason rather than pretending.

`--repository` is the URL a published client reads and it is embedded. It has to be
the address clients will use, not the path this build happens to write to; it
defaults to the staged tree as a `file:` URL, which is right for a local origin and
obviously wrong for a real one. `--thin-output` chooses where the two installers go,
defaulting to the directory beside the web tree.

`--channel` must match the manifest's `[updates] channel`. A build that let them
differ would publish a launcher pointing at a document nobody signed.

A project that wants no bucket and no CDN can publish the release on GitHub
instead - `zup publish stage --packages dist/packages` then `zup publish github` -
which travels one asset per variant rather than one per content object. See
[GitHub distribution](github-distribution.md).

## The two thin installers

`--thin` emits two files that are one artifact with two promises:

```text
Acme-Setup-version.exe   reads releases/stable/versions/1.4.0.json
Acme-Setup-channel.exe   reads releases/stable.json
```

A version-labelled installer always installs the release it was built for, because
it reads an immutable name that nothing rewrites. A channel installer installs
whatever the channel currently says. Everything else about them is identical,
including the bytes.

A thin release's runtime is not the template. It is the template with this target's
plan compiled into it and none of the content - a few megabytes, and it knows
exactly what it would install. That image is staged into the web tree as ordinary
verified content, because a bootstrapper that fetched something the graph did not
name would have nothing to check it against.

## Sign with tuftool

Keep signing keys outside zup. `zup publish stage` writes the documents into
`tuf-input` in exactly the shape `--add-targets` reads:

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

Configure role thresholds and keys in `root.json` for the deployment; keep root and
targets signing offline and use a restricted online timestamp key for timestamp
refreshes. Root rotation is published through the normal versioned TUF root chain,
and shipped out-of-band in newly built installers; existing clients follow and
verify rotations from their embedded trusted root.

## What the client does

`update check` reads the signed channel target and reports the current release.
`update` resolves the release graph, selects the variant this machine already runs,
compares the required digests against its verified cache, downloads only the
missing blobs, and runs the ordinary upgrade lifecycle against them. A person runs
either from the persisted `maintenance.exe` or from a newly downloaded
`Acme-Setup.exe`; neither downloads a new installer.

Both accept `--scope`, `--state-root`, `--output`, `--non-interactive`, and
`--yes`. `--source` points at a local directory that satisfies the content side of
the closure, and it is verified exactly like anything off the network: a seed is a
place to fetch from, never a place to trust.

Safe TUF expiration checks are always enabled. Timestamp, snapshot, and targets
metadata are persisted at `<update-state-root>/updates/<app-id>/<channel>/tuf/` -
do not delete that directory to recover from a verification failure. The datastore
is namespaced by the trusted root's digest, the repository's fingerprint, and the
channel, so two repositories or two channels cannot poison each other's rollback
memory, and trusted metadata is never deleted on failure.

For machine installs, the invoking user's `%LOCALAPPDATA%\zup` is the update state
root, so metadata and verified content are writable before the existing lifecycle
requests elevation.

Target bytes stay in a private `.partial` file in the per-user content cache until
the complete stream hashes to the digest the release authenticated, and are then
published by an atomic rename. The downloaded installer publishes itself as
maintenance only if its transaction commits.

A downgrade is refused: the lifecycle requires the new package version to be
greater than the installed version, an equal version is a modify, and a lower
version is an error. There is no manifest option to allow it.

Install, update, repair, and modify are the same call with a different closure -
see [online acquisition](online-acquisition.md#one-graph-four-closures).
