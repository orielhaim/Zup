# Build

Build after `check` and `doctor` are clean.

```bash
zup build
```

With one selected target and no explicit artifact profiles, this produces one installer for that target.

## Toolchain

`zup build` composes an installer from binaries built once and shipped beside the CLI: a runtime template per target and frontend, launchers, and the preset that draws the window. Zup looks for them in this order:

1. `--toolchain <DIR>` or `ZUP_TOOLCHAIN`
2. the per-machine cache (`zup toolchain install <release-dir>`)
3. `toolchain/<version>/` beside the executable

Each file carries a `.zup-toolchain.json` descriptor with its version, target, frontend and digest. A component from another Zup version is refused. `zup toolchain status` reports what is present and where.

Building Zup from source also needs:

```bash
cargo xtask toolchain build
```

Without it, builds fail with `zup.toolchain.component_missing`.

## Select targets

```bash
zup build --target windows-x64
```

`--target` names target profiles or canonical triples. It chooses native variants to build; it does not name the final distribution artifact.

Linux targets build the same way, on either host:

```bash
zup build --target x86_64-unknown-linux-gnu
```

A Linux build writes one self-contained, extensionless installer per target (for example `Acme-Setup`). Universal and thin artifacts are Windows installers: a Linux target in an artifact composition is refused with the per-target alternative spelled out.

## Explicit artifacts

When one release needs a specific file layout, declare `[build.artifacts.*]` profiles and build them by name:

```bash
zup build --artifact desktop
```

Artifact profiles control single vs universal and offline vs thin packaging. Keep those release choices out of basic target configuration.

See [Artifacts](/ship/artifacts).

## Inspect output

```bash
zup artifact inspect Acme-Setup.exe
```

Inspection describes the artifact and its verification state without installing it.

## Release description

Builds write `zup-release.json` by default. Signing and publishing consume that release description rather than rediscovering files from a directory.

The next step for a public release is [Signing](/ship/signing).
