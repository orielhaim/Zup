# Troubleshooting

## The toolchain is missing

```text
no runtime template for `gui` on x86_64-pc-windows-msvc was found.

  Build the zup toolchain for this version and stage it, or point zup at one:
    cargo xtask toolchain build
    zup --toolchain <dir> ...   (a directory of components)
```

`zup build` composes an installer out of binaries that were built once, and this
Zup release has not found them. Check where it looked:

```bash
zup toolchain status
```

It reports every component, whether it was found, and which of the four sources
it came from: an `override` from `--runtime`, a `root` from `--toolchain` or
`ZUP_TOOLCHAIN`, the `cache`, or `staged` beside the executable.

A toolchain is usable only by the Zup release that produced it. Each file
carries a `.zup-toolchain.json` descriptor naming the version, target and
frontend it was built for. Copying a toolchain between Zup versions, or
hand-editing a descriptor, produces exactly this error.

## The target is refused

```text
unsupported backend for target `aarch64-apple-darwin`: backend not implemented
```

The triple names a platform Zup has no backend for. The refusal happens at the
backend boundary, before your payload is read, so nothing is written.

Check the architecture first: `arm64-` is rewritten to `aarch64-`, and `x64-` to
`x86_64-`, so a spelling that looks unusual is usually the canonical one.

A different message means the platform has a backend but this build host does not
meet its requirements:

```text
backend unavailable: target lowering for `x86_64-pc-windows-msvc` requires a Windows build host
```

`zup doctor` reports the same thing as a check rather than an error, which is the
place to confirm it.

## The manifest is rejected

Every unknown key is an error, including one you added for a future release. The
diagnostic names the key.

### A key you expected is not there

Check the spelling against the [manifest reference](/reference/manifest). Renamed
keys from earlier schema 1 drafts are refused rather than aliased, apart from
`allow_install_directory` and `allow_install_dir`, which are accepted aliases for
`allow_directory_override`.

### An install directory is not specified

```text
install directory for scope `user` is not specified
```

`[install.directory]` must cover every scope your `scope` allows. `either` needs
both `user` and `machine`.

### A template names something unknown

```text
unknown template variable `known.program_files`
```

See [path templates](/configure/install#path-templates) for the complete list. The
`${known.*}` family was removed.

### A component dependency cycle

```text
component dependency cycle: editor → plugins → editor
```

### A required component defaults to off

```text
required component `program` cannot default to disabled
```

### An artifact declares a file name, not one

```text
artifact `windows` declares output `dist/Acme.exe`, which is not a file name
```

`output` is a bare file name. Use `--output` to write somewhere else.

## A pattern matched nothing

```text
pattern `docs/**` matched no files
```

Either the directory does not exist or the pattern is wrong. If the directory
legitimately might be empty, say so:

```toml
allow_empty = true
```

## The window does not appear

```text
target `windows` uses the `headless` frontend; `--target` names one that presents a window
```

`preview` needs a `gui` or `console` target, and a preset package. A machine with
no staged preset cannot draw a window. See
[install Zup](/getting-started/install).

## A settings change is ignored

Check what the preset actually accepts:

```bash
zup preset inspect ./acme-brand.zupui
```

The output lists the settings it takes, and whether it permits others. A key that
validates but is not listed is accepted and never read - the schema allows
unknown properties, and the preset does not use them.

A setting that fails validation is reported with its path, and the last settings
that fitted stay in force:

```text
window   acme-brand does not accept the configured settings: ui.settings.accent: string is not valid under any of the given schemas; the last valid preview is still running
```

## Signing is refused

### The chain is not trusted

```text
zup.signing.untrusted_chain
```

A development certificate does not chain to a root Windows trusts. Produce a
relaxed plan:

```bash
zup sign prepare --release-dir dist --allow-untrusted-chain --allow-missing-timestamp
```

The relaxation travels in `zup-signing.json`, so `zup sign verify` needs no flags.

### A signature has no timestamp

```text
zup.signing.timestamp_missing
```

Production signatures need an RFC 3161 timestamp. Add `/tr` to your signing
command.

### The artifact embeds an unsigned runtime

```text
embeds `windows-x64`, which is unsigned or missing; a signature on the container does not travel with the executable extracted from it
```

A `universal` artifact contains a runtime. Sign the runtime, put the signed copy
at `dist/runtime/<variant>.exe`, and re-compose - then sign the artifact. Signing
only the outer `.exe` is not enough.

### The release was not finalized

```text
this release has not been signed and finalized: `windows` does not carry pre-signature digests
```

`zup publish github` refuses this before any network call. Run
`zup sign verify --release-dir dist` first, or `--allow-unsigned` if the release
genuinely is unsigned.

## Publishing is refused

### No credential

```text
zup.publish.no_credential
```

Set `GH_TOKEN` or `GITHUB_TOKEN`, or run `gh auth login`. A manifest cannot carry
a token - it is committed, so a token in one reaches every fork.

### Too many assets, or one too large

GitHub allows 1000 assets and 2 GiB per asset. Zup checks both before writing
anything. If an asset is too large, split it with
`--packages` and a lower `--shard-bytes`; if there are too many, you are
publishing content objects rather than packages.

### An asset conflicts

```text
zup.publish.asset_conflict
```

On a draft, `--replace-conflicts` replaces a differing asset. Read the diff first.
On a published release, replacing is refused: those bytes are already public.

## An update does not find a release

```text
release target `releases/stable.json` is missing
TUF repository verification failed: ...
```

- The repository URL is the address clients use, not the path you staged to.
  Check `--repository` and `[updates] repository`.
- TUF metadata is not written by Zup. It has to be signed with `tuftool` and
  served - see [static hosting](/ship/web#what-belongs-on-the-origin).
- The channel must match. A thin installer built for `beta` will not read
  `releases/stable.json`.

A refusal that names an application or a version means the repository is serving
somebody else's release, or a version other than the one the installer expects.
Zup refuses rather than installing it.

## A CI build fails with a toolchain error

A build job runs on a fresh runner, and a fresh runner has no toolchain. Check
what the job resolved:

```yaml
- uses: orielhaim/zup@action-v1
  with:
    operation: setup
- run: zup toolchain status
```

If the runner has none, either use a self-hosted runner that has one, or build one
in the job with `cargo xtask toolchain build` from a checkout of Zup itself and
pass `zup-path`.

`publish` and `release` also refuse to run on `pull_request_target` or
`workflow_run`, because those events hand the job the base repository's secrets
while running code that may have come from a fork:

```text
publish cannot run on `pull_request_target`: that event gives this job the base
repository's secrets and write token while running code that may have come from
a fork.
```

`build`, `compose` and `attest` are not refused on those events.

## A plugin fails to build

```text
zup.check.plugin_compile_failed: plugin `acme-integrations` exhausted its fuel budget
plugin planning timed out
plugin exceeded a sandbox resource limit
```

A plugin gets 250 ms of wall clock, 32 MiB of memory, and no host calls at all -
no filesystem, no network, no clock, no randomness. Anything it needs must come
from the eight fields of its context, or be returned as a generated file's
contents. See [plugins](/advanced/plugins#what-it-cannot-do).

## Diagnosing a failure in CI

Every diagnostic carries a code, a message, and usually a source location. Match
on the code, not the message:

```bash
zup build --format json
```

```json
{
  "diagnostics": [{
    "severity": "error",
    "code": "zup_manifest::missing_install_directory",
    "message": "install directory for scope `user` is not specified",
    "source": { "file": "zup.toml", "start_line": 21, "start_column": 1 },
    "help": "set [install.directory] `user` and/or `machine` to cover the configured scope"
  }]
}
```

`zup check` and `zup doctor` produce the same diagnostics without writing
anything, so a CI step that fails can be reproduced locally.

The full contract is in [machine-readable output](/advanced/automation).
