# Package a preset

A preset package is a `.zupui` containing the preset description and one or more native target binaries.

## Build and pack locally

```bash
zup ui pack --build x86_64-pc-windows-msvc
```

`--build` asks Zup to build the preset for that target with Cargo and include it.

For a binary built elsewhere:

```bash
zup ui pack \
  --binary x86_64-pc-windows-msvc=dist/aurora.exe \
  --binary aarch64-pc-windows-msvc=dist/aurora-arm64.exe
```

Use both forms in CI when targets require different build machines.

## Choose the output

```bash
zup ui pack --build x86_64-pc-windows-msvc -o dist/aurora.zupui
```

The default Cargo profile is `release`. `--force` permits replacing an existing package.

## Inspect without running

```bash
zup ui inspect dist/aurora.zupui
```

Inspection reports package identity, accepted settings and included targets without launching the preset.

## Use it

```toml
[ui]
preset = "./ui/aurora.zupui"
```

One application path can therefore serve a target matrix. The build picks the preset binary that matches the application target.

Keep preset packages versioned like any other application build input. If the UI changes, the installer artifact changes even when the payload does not.
