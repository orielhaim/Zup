# Quick start

This guide assumes `zup` is available on `PATH`.

## Create a project

```bash
zup init --name Acme --app-id com.acme.desktop
```

`zup init` writes an editable `zup.toml` and a source directory. The generated manifest is deliberately small.

A minimal project looks like this:

```toml
#:schema https://zup.orielhaim.com/schema/zup.toml.json

schema = 1

[app]
id = "com.acme.desktop"
name = "Acme"
version = "1.0.0"
main = "Acme.exe"

[build.targets.windows]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist" }

[install]
scope = "user"

[install.directory]
user = "${location.user_data}/Acme"
```

Put the built application in `dist/`.

## Validate it

```bash
zup check
zup doctor
```

`check` validates the manifest and the files it will ship. `doctor` checks whether the selected targets can be built on the current machine.

Fix both before building.

## Preview the installer

```bash
zup preview
```

Preview opens the installer UI over a simulated machine. It does not install application files or register system resources.

Use it to check application metadata, scope choices, components and the selected preset before producing an installer.

## Build

```bash
zup build
```

With one target and no explicit artifact profiles, Zup builds one installer for that target. The build also writes `zup-release.json` unless you override or disable that output.

Use [Build](./build) when you need multiple targets or explicit artifacts. Use [Ship](/ship/) when the installer is ready to sign and publish.
