# Quick start

This guide assumes `zup` is on `PATH`.

## Create a project

```bash
zup init
```

The wizard asks four questions, one at a time: application name, application ID,
target platforms, and source directory. The name defaults to the project directory,
and the ID follows the name. The host platform is selected by default. Use the arrow
keys to move, Space to select targets, and Enter to continue.

Zup uses an existing `dist`, `build`, or `out` directory when available. Otherwise,
it uses the project directory if it contains one compatible executable, or creates
`dist`. It writes one `[build.targets.<target>]` profile per selected target.

To generate a manifest immediately with these defaults:

```bash
zup init -y
```

Command-line values override the defaults and skip their questions:

```bash
zup init --yes --name Acme --app-id com.acme.desktop \
  --target windows-x64 --target linux-arm64 --source dist
```

| Flag | Meaning |
| --- | --- |
| `-y`, `--yes` | accept defaults without prompting |
| `--name`, `--app-id` | display name and application ID |
| `--target` | target platform; repeat or use a comma-separated list |
| `--version` | starting version (default `0.1.0`) |
| `--source` | payload directory |
| `--main` | existing entry point relative to the source directory |
| `--scope` | `user` (default), `machine`, or `either` |
| `--frontend` | `gui` (default), `console`, or `headless` |
| `--manifest` | where to write (default `zup.toml`) |
| `--force` | replace an existing manifest |

Version, scope, and frontend are not wizard questions. Zup sets `app.main` only
when it finds exactly one compatible executable in the source directory, or when
you supply `--main` with an existing file. With no entry point, it omits `main`.
For multiple targets, adjust each profile's source directory if their payloads
differ.

A Windows x64 project with an existing `dist/Acme.exe` can look like this:

```toml
#:schema https://zup.dev/schema/zup.toml.json

schema = 1

[app]
id = "com.acme.desktop"
name = "Acme"
version = "0.1.0"
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
