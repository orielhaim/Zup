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

Packaging runs one native binary in describe mode and reads the document it
prints. A binary for another target cannot run here, so packing one needs its
describe document from a native run:

```bash
aurora-linux --zup-describe > describe.json
zup preset pack \
  --binary linux-x64=dist/aurora-linux \
  --describe-file describe.json
```

The document is validated and cross-checked against the Cargo manifest exactly
as a live describe run would be. Every binary in a package describes the same
logical preset: one name, one version, one protocol, one settings schema.

## Build a Linux GUI installer

A Linux GUI installer is an ordinary build target. On a Linux host with the
toolchain staged (`cargo xtask toolchain build`), this composes a graphical
installer from the same Default Preset sources Windows uses:

```bash
zup build --manifest path/to/zup.toml --target linux-x64
```

The frontend comes from the manifest (`gui`, `console`, or `headless`); a
`gui` build resolves the preset package, selects the `linux-x64` binary, and
embeds it alongside the package. Composing a Linux GUI installer on Windows
works the same way from a multi-target preset package (see `--describe-file`
above): composition copies target binaries, it never executes them.

Running a GUI installer with no graphical session is refused with a
diagnostic; it never silently falls back to headless. It needs Wayland
(`WAYLAND_DISPLAY`) or X11 (`DISPLAY`).

## Native dependencies for Linux GUI builds

Cross-compiling a GPUI preset is not supported: building a Linux GUI preset
needs a Linux host with the GUI system libraries installed (fontconfig,
freetype, Wayland, X11, xkbcommon). On Debian/Ubuntu:

```bash
sudo apt install pkg-config libfontconfig1-dev libfreetype-dev libwayland-dev libx11-dev libxkbcommon-dev libxkbcommon-x11-dev libxcb1-dev
```

Packing a preset for a target whose binary cannot run here needs
`--describe-file` (above). Composing an installer from staged binaries never
needs those libraries; only building the preset itself does.

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
