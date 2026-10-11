# Build

Build after `check` and `doctor` are clean.

```bash
zup build
```

With one selected target and no explicit artifact profiles, this produces one installer for that target.

## Select targets

```bash
zup build --target windows-x64
```

`--target` names target profiles or canonical triples. It chooses native variants to build; it does not name the final distribution artifact.

Linux targets build the same way, on either host:

```bash
zup build --target x86_64-unknown-linux-gnu
```

A Linux build writes one self-contained, extensionless installer per target (for example `Acme-Setup`). Universal and thin artifacts are Windows-only: a Linux target in an artifact composition is refused with the per-target alternative spelled out.

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
