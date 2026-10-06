# Package a preset

A preset package is a `.zupui` containing the preset's description and one or
more native target binaries.

## Build and pack locally

```bash
zup preset pack --build x86_64-pc-windows-msvc
```

`--build` asks Zup to build the preset for that target with Cargo and include it.
Pass `--manifest <dir>` to pack a project somewhere other than the current
directory.

For a binary built elsewhere:

```bash
zup preset pack \
  --binary x86_64-pc-windows-msvc=dist/aurora.exe \
  --binary aarch64-pc-windows-msvc=dist/aurora-arm64.exe
```

Use both forms in CI when targets require different build machines. At least one
source is required.

## What packing does to the preset

`zup preset pack` runs the preset executable in its describe mode and reads the
document it prints. That is the only time a preset runs outside an installer,
and it is where the package's settings schema comes from - generated from your
`Settings` type, so it cannot drift from what the preset deserializes at runtime.

The name and version in that document are cross-checked against the project's
Cargo manifest. A preset that hard-codes its identity, or bumps its version
without bumping the manifest, is refused rather than packaged under a name no
crate answers to.

## Choose the output

```bash
zup preset pack --build x86_64-pc-windows-msvc -o dist/aurora.zupui
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
  x86_64-pc-windows-msvc        4.2 MiB  sha256:...
```

That reports identity, the preset protocol the package speaks, required
capabilities, the settings it accepts, and every target with its size and digest
- without launching anything.

## Use it

```toml
[ui]
preset = "./ui/aurora.zupui"
```

One application path can therefore serve a target matrix. The build picks the
preset binary that matches the application target.

Keep preset packages versioned like any other application build input. If the
installer window changes, the installer artifact changes even when the payload
does not.