# Quick start

This guide assumes `zup` is on `PATH`.

## Create a project

```bash
zup init --name Acme --app-id com.acme.desktop
```

`zup init` writes an editable `zup.toml` and a source directory. Without flags it asks for the values; in a script pass them:

```bash
zup init --name Acme --app-id com.acme.desktop --version 1.0.0 \
  --source dist --main Acme.exe --scope user --non-interactive
```

| Flag | Meaning |
| --- | --- |
| `--name`, `--app-id` | display name, reverse-DNS id (required) |
| `--version` | starting version (default `0.1.0`) |
| `--source` | payload directory (default `dist`) |
| `--main` | main executable |
| `--scope` | `user`, `machine`, `either` |
| `--frontend` | `gui`, `console`, `headless` |
| `--manifest` | where to write (default `zup.toml`) |
| `--force` | replace an existing manifest |

A minimal project looks like this:

```toml
#:schema https://zup.orielhaim.com/schema/zup.toml.json

schema = 1

[app]
id = "com.acme.desktop"
name = "Acme"
version = "1.0.0"
main = "Acme.exe"

[build.targets.windows-x64]
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

Fix both before building.

## Preview the installer

```bash
zup preview
```

It does not install anything. Use it to check metadata, scope choices, components and the selected preset before building.

## Build

```bash
zup build
```

With one target and no explicit artifact profiles, Zup builds one installer for that target. The build also writes `zup-release.json` unless you override or disable that output.

Use [Build](./build) when you need multiple targets or explicit artifacts. Use [Ship](/ship/) when the installer is ready to sign and publish.
