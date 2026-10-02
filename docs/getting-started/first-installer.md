# Create an installer

An installer is described by a `zup.toml` next to the files it ships. Zup can
write the first one for you.

## Create the project

```bash
zup init --name Acme --app-id com.acme.desktop
```

`zup init` writes `zup.toml` and creates the source directory it names. Run it
without flags and it asks for the name, identifier, source directory, install
scope and main executable. In a script, pass what it would have asked for:

```bash
zup init --name Acme --app-id com.acme.desktop --version 1.0.0 \
  --source dist --main acme.exe --scope user --non-interactive
```

| Flag | Meaning |
| --- | --- |
| `--name` | The display name, e.g. `Acme` |
| `--app-id` | A reverse-DNS identifier, e.g. `com.acme.desktop` |
| `--version` | The version to start at. Default `0.1.0` |
| `--source` | The directory the payload lives in. Default `dist` |
| `--main` | The file Windows launches. Default `app.exe` |
| `--scope` | `user`, `machine` or `either` |
| `--frontend` | `gui`, `console` or `headless` |
| `--manifest` | Where to write it. Default `zup.toml` |
| `--force` | Replace a manifest that already exists |
| `--non-interactive` | Never ask a question |

`--name` and `--app-id` are required in non-interactive mode. A script that did
not say what the application is called cannot be answered for it.

## What it wrote

```toml
#:schema https://zup.orielhaim.com/schema/zup.toml.json

schema = 1
frontend = "gui"

[app]
id = "com.acme.desktop"
name = "Acme"
version = "0.1.0"
main = "app.exe"

[build]

[build.targets.default]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist" }

[install]
scope = "user"
allow_directory_override = true

[install.directory]
user = "${location.user_data}/acme"
```

Four parts, and only four:

- `[app]` - what the application is called and which file Windows launches.
- `[build.targets.default]` - which target to build and where its files are.
- `[install]` - who the application installs for.
- `[install.directory]` - where it goes.

The empty `[build]` header is not required; it is here so the profiles below it
can be written as dotted keys.

Put your built application into `dist/`. The `source` directory is copied into
the installer as it stood when you built it, so build the application first.

## Validate it

```bash
zup check
```

`zup check` reads the manifest, resolves the target, walks the payload, compiles
any plugins and reports what a build would do. It writes nothing.

```text
✓ Acme is valid (default)
  Target      x86_64-pc-windows-msvc
  Source      dist
  Install     user · user=${location.user_data}/acme
  Components  0
  Files       3
  Plugins     0
```

Every undeclared key is an error, including one you meant for a future release.
A manifest that parses but does not make sense fails here rather than at the end
of a build.

## See the window

```bash
zup preview
```

This opens your application's actual installer window over a simulated machine.
The preset, the settings and the state are the ones a build would compose - it
is not a drawing of your installer. It is a safe place to work: a preview writes
nothing outside its own `.zup/` directory, so no file, shortcut, `PATH` entry or
registry value is touched.

Leave it open and edit `zup.toml`. The window re-resolves when you save.

Next: [build and test](/getting-started/build-and-test).
