# Package a preset

A preset package is a `.zupui` containing the preset's description and one or
more native target binaries.

## Build and pack locally

```bash
zup preset pack --build windows-x64
```

`--build` asks Zup to build the preset for that target with Cargo and include it.
Pass `--manifest <dir>` to pack a project somewhere other than the current
directory.

For a binary built elsewhere:

```bash
zup preset pack \
  --binary windows-x64=dist/aurora.exe \
  --binary windows-arm64=dist/aurora-arm64.exe
```

Use both forms in CI when targets require different build machines. At least one
source is required.

## What packing does

`zup preset pack` runs the preset in describe mode and reads the document it prints: settings schema, identity, capabilities. The name and version are cross-checked against the Cargo manifest.

## Choose the output

```bash
zup preset pack --build windows-x64 -o dist/aurora.zupui
```

The default Cargo profile is `release`. `--force` permits replacing an existing
package.

## Inspect without running

```bash
zup preset inspect dist/aurora.zupui
```

```text
preset        aurora 1.0.0
package       schema 1
preset protocol   1
capabilities  none required
settings      accent, hero, logo (others permitted)

targets
  windows-x64        4.2 MiB  sha256:...
```

That reports identity, protocol, capabilities, accepted settings, and every target with size and digest - without launching anything.

One application path can therefore serve a target matrix: the build picks the
preset binary that matches the application target.
