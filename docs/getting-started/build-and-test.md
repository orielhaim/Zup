# Build and test

## Validate

```bash
zup check
```

Reads the manifest, resolves the target, walks the payload, compiles plugins and
reports what a build would do. Writes nothing. Use it in a pre-commit hook or a
CI step.

## Check build readiness

```bash
zup doctor
```

`doctor` answers one question: would `zup build` succeed right now. It checks
that the toolchain components for the selected targets are present and verified,
that the target has a backend, that the payload exists, and that the output path
is writable.

`doctor` is read-only and safe to run anywhere. Unlike `check`, an incomplete
toolchain is a failure it reports rather than something it works around.

## See what installing would change

```bash
zup plan
```

Prints the installation plan without touching the machine - every file, every
shortcut, every `PATH` entry, with resolved absolute paths.

```text
Acme 1.4.0
Scope: user
Location: C:\Users\you\AppData\Local\acme
Install: 1.9 KiB

Files
  Create acme.exe - File { destination: "C:\\Users\\you\\AppData\\Local\\acme\\acme.exe" }
```

Useful flags:

| Flag | Effect |
| --- | --- |
| `--scope user` \| `--scope machine` | Plan for a scope. Default `user` |
| `--enable ID` | Add a component to the plan. Repeatable |
| `--disable ID` | Remove one. Repeatable |
| `--state-root DIR` | Read the existing installation from here |

## Build

```bash
zup build
```

Writes the installer and a release description. With one target and no declared
artifacts, expect:

```text
→ Validating manifest and resolving the toolchain
→ Materializing payload and compiling plugins
→ Compressing and embedding default for x86_64-pc-windows-msvc

Built Acme 1.4.0 (default)
  Frontend    gui
  Installer   Acme-Setup.exe
  Size        91.2 MiB
  Target      x86_64-pc-windows-msvc
  Payload     3 files · 1.9 KiB
  Plugins     0
  Updates     not configured

→ Wrote C:\projects\acme\Acme-Setup.exe
→ Wrote C:\projects\acme\zup-release.json

Sign them, then run `zup sign verify` to finalize the release.
```

Two files, and both matter:

- `Acme-Setup.exe` - what a user downloads and runs.
- `zup-release.json` - what was published, with a digest per artifact. Signing
  and publishing both read it. A release without one cannot be verified by
  anyone.

Without `--output`, artifacts are written beside `zup.toml`. The name depends on
what you are building, and carries the target's own executable suffix (`.exe`
on Windows, none on Linux):

| You are building | Name |
| --- | --- |
| One target, no declared artifacts | `Acme-Setup.exe` |
| Several targets, no declared artifacts | `Acme-Setup-<profile>.exe` |
| A declared `universal` artifact | `Acme-Windows-Setup.exe` |
| A declared `single` artifact | `Acme-Setup.exe` |
| A declared artifact with a channel | `Acme-Windows-stable-Setup.exe` |
| One Linux target, no declared artifacts | `Acme-Setup` |

An existing output is refused rather than replaced:

```text
output `Acme-Setup.exe` already exists; pass --force to overwrite it
```

`--force` composes to a staging file and renames it into place, so an interrupted
build never leaves a half-written installer where the next one expects a whole
one.

### Useful build flags

| Flag | Effect |
| --- | --- |
| `--output PATH` | Where to write the artifact. Repeatable; one per artifact |
| `--target NAME` | Build one profile. Repeatable. Empty means every profile |
| `--artifact ID` | Build one declared artifact. Repeatable |
| `--universal` | Compose one artifact from every selected target |
| `--force` | Replace an output that already exists |
| `--release-manifest PATH` | Where to write the release description, or `none` |
| `--signing-subject SUBJECT` | The publisher name a signature must carry |

`--target` names a native variant to build. `--artifact` names a file a user
downloads. They are different questions, and `--artifact` cannot be combined
with `--universal`.

## Test the installer

Run `Acme-Setup.exe`. It is a real installation, not a simulation.

What to check, in order:

- The window shows your application's name, version, publisher, description and
  components.
- The location line shows where it will install.
- Install, then find the application in the Start menu.
- Run the installer again. It becomes the maintenance tool: the same executable
  that installed the application now changes its components, repairs drifted
  files and uninstalls it.
- Uninstall. The files, the shortcuts and the `PATH` entry all go.

`zup preview` covers the states that are hard to reach on demand - a file in the
way, a failed transaction, a machine that must restart, a component that drifted
- because it can be driven into each of them on purpose.

## Format

```bash
zup fmt            # rewrite zup.toml, preserving comments
zup fmt --check    # report whether it is formatted, write nothing
```

Next: [app metadata](/configure/app).
