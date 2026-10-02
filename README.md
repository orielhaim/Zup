# zup

A programmable application installer for the modern desktop. Windows is the only
implemented backend; the portable stack builds and tests on Linux, which is what
keeps the Windows code an adapter.

Every internal format is version 1 and an incompatible change is expected without
a version bump.

## Quickstart

```powershell
cargo run -p zup -- init --name Acme --app-id com.acme.desktop --non-interactive
cargo run -p zup -- check
cargo run -p zup -- doctor
cargo run -p zup -- build
```

`init` writes `zup.toml`:

```toml
#:schema https://zup.dev/schema/zup.toml.json

schema = 1

[app]
id = "com.acme.desktop"
name = "Acme"
version = "0.1.0"
main = "app.exe"
icon = "assets/icon.svg"

[build]

[build.targets.default]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist" }

[install]
scope = "user"

[install.directory]
user = "${location.user_data}/acme"
```

`schema`, `[app]`, `[build]`, and `[install]` are required. `[build]` declares at
least one target profile; the empty header is only needed when profiles are
written as dotted keys. The parser rejects every undeclared key. `--target` is
repeatable and accepts a profile name or a canonical triple.

`doctor` is read-only: it answers whether `build` would succeed now.

## The toolchain

`zup build` composes an installer out of zup's own binaries - a runtime template
for the target's machine and presentation, plus a launcher - so it needs a
toolchain. Installers ship one beside the executable. Contributors build one:

```text
cargo xtask toolchain build             # for `cargo run` and `cargo test`
cargo xtask toolchain build --profile release
```

## Authoring commands

| Command | Purpose |
| --- | --- |
| `zup init` | Write a small, editable `zup.toml` and its source directory |
| `zup check` | Validate a manifest and its build inputs |
| `zup doctor` | Report build readiness for the selected targets |
| `zup plan` | Print the installation plan without touching the machine |
| `zup build` | Produce a self-contained installer |
| `zup artifact inspect` | Describe a built artifact and how it verifies |
| `zup sign prepare` / `verify` | Bracket your own signer; zup holds no key |
| `zup publish stage` | Write the web tree a static origin serves |
| `zup publish github` | Publish a staged release to GitHub |
| `zup ci github` | Generate or check the release pipeline |
| `zup schema` | Print or write the authoritative JSON Schema |
| `zup fmt` | Format `zup.toml`, preserving comments |
| `zup completions <shell>` | Shell completions to stdout |

`zup` is the only binary a person installs. The application runtime is a
different package with different verbs (`install`, `modify`, `repair`, `update`,
`uninstall`) and reaches users inside a generated `Acme-Setup.exe`. It persists
as `maintenance.exe`.

## Releasing

```yaml
permissions:
  contents: write
steps:
  - uses: actions/checkout@v7
  - uses: orielhaim/zup@action-v1
    with:
      operation: release
```

The publishing token reaches only `zup publish github`'s environment - never a
build, which may run arbitrary project build scripts.

Signing keeps the key outside zup:

```text
zup sign prepare --release-dir dist
zup sign verify  --release-dir dist
```

## Repository tasks

```text
cargo xtask toolchain build              # build runtime templates and launchers
cargo xtask toolchain package            # assemble the release directory
cargo xtask release clean-room           # prove it works outside this repo
cargo xtask verify-portable-boundaries   # portable crates reach no Windows code
cargo xtask verify-dependency-graph      # two versions of one crate; dev tools in shipped binaries
cargo xtask automation generate|check    # protocol artifacts from the Rust DTOs
cargo xtask github-action-pins check|refresh
```

These are real gates in CI, not conventions. Each fails the build.

The GitHub Action is developed in [`action/`](action) with Bun as the toolchain
and Node 24 as the runtime:

```bash
cd action && bun install --frozen-lockfile && bun run check
```

## Documentation

**System** - [architecture](docs/architecture.md) (pipeline, crates, portable
boundary, persisted formats) · [artifact graph](docs/artifact-graph.md) ·
[installer frontends](docs/frontends.md) ·
[online acquisition](docs/online-acquisition.md) · [plugins](docs/plugins.md)

**Shipping** - [signing](docs/signing.md) · [GitHub Action](docs/action.md) ·
[GitHub distribution](docs/github-distribution.md) · [updates](docs/updates.md)

**Integrating** - [automation protocol](docs/automation.md)

**Trust** - [security model](docs/security.md)
