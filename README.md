# zup

A programmable application installer for the modern desktop.

Zup is in very early development. The public API is not available yet.

Windows is the only implemented platform backend. A manifest that declares a
non-Windows target is refused at an explicit boundary before the source tree is
walked; a Windows target on a non-Windows build host is refused the same way.
Nothing is written in either case. The portable stack still builds and tests
natively on Linux, which is what keeps the Windows code an adapter.

## Quickstart

Create a manifest and a source directory:

```powershell
cargo run -p zup -- init --name Acme --app-id com.acme.desktop --non-interactive
```

`zup init` writes `zup.toml`:

```toml
#:schema https://zup.dev/schema/zup.toml.json

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

Every manifest declares `schema = 1` and at least one entry under
`[build.targets.<profile>]`. A profile is a friendly name; its `target` is a
canonical triple from `target-lexicon`, such as `x86_64-pc-windows-msvc` or
`aarch64-pc-windows-msvc`. A resource may narrow itself to named profiles with
`targets = ["<profile>"]`.

Check and build:

```powershell
cargo run -p zup -- check
cargo run -p zup -- doctor
cargo run -p zup -- build
```

`zup check` validates the manifest and its build inputs. `zup doctor` answers
whether `zup build` would succeed right now, without writing anything. `zup build`
produces a self-contained installer named `<App>-Setup.exe`, or
`<App>-Setup-<profile>.exe` when more than one profile is selected.

A build composes zup's own binaries — a runtime template for the target's
machine and presentation, and a launcher for a universal artifact — so it needs a
**toolchain**. A person who installed `zup` already has one, staged beside the
executable. A contributor working in this repository builds one:

```text
cargo xtask toolchain build          # for `cargo run` and `cargo test`
cargo xtask toolchain build --profile release
```

The command builds the three runtime templates and the four launchers, names each
one for the machine and presentation it is for, and writes a machine-readable
descriptor beside it. `zup build` then finds every component it needs without
being told where they are, and refuses any component that was produced by a
different zup release, for a different machine, or for a different presentation —
a check the descriptor and the file's own header make possible on a build host
that cannot run the file. See [the toolchain contract](docs/frontends.md).

## Authoring commands

| Command | Purpose |
| --- | --- |
| `zup init` | Write a small, editable `zup.toml` and its source directory |
| `zup check` | Validate a manifest and its build inputs |
| `zup doctor` | Report build readiness for the selected targets |
| `zup plan` | Print the real installation plan without touching the machine |
| `zup build` | Produce a self-contained installer executable |
| `zup artifact inspect` | Describe a built artifact and how it verifies |
| `zup publish stage` | Write the web tree a static origin serves |
| `zup publish github` | Publish a staged release to GitHub |
| `zup ci github` | Generate or check the release pipeline a project commits |
| `zup schema` | Print or write the authoritative JSON Schema |
| `zup fmt` | Format `zup.toml` while preserving comments |
| `zup completions <shell>` | Write shell completions to stdout |

`zup` has **no Cargo features**. It is one tool with one shape, so
`cargo run -p zup -- --help` and `cargo install --path crates/zup` need no flag
deciding what you get.

`--target` is repeatable on `zup check`, `zup doctor`, `zup plan`, and
`zup build`, and accepts a profile name or a canonical triple. On `zup check`,
`zup doctor`, and `zup build`, `--source` and `--install-directory` are
repeatable and positional against the selected targets, in selection order. An
empty `--target` selects every profile. `zup plan` takes one target.

See [architecture](docs/architecture.md) for the CLI surface in full.

## The two executables

`zup` is the developer tool. It is the only binary in this repository a person
installs. The application runtime is a different package with a different command
surface, and it reaches users inside a generated `Acme-Setup.exe` rather than on a
`PATH`:

| | `zup` | `Acme-Setup.exe` |
| --- | --- | --- |
| Acts on | a project directory | an installed application |
| Verbs | `init check doctor plan build artifact publish ci schema fmt completions` | `install modify repair update uninstall` |
| Knows about | `zup.toml`, source trees, releases | its own application, and nothing else |
| Build flags | none | `--output human\|json\|jsonl`, `--yes`, `--scope` |

`upgrade` and `recover` are reachable as hidden `__upgrade` and `__recover` for
the contracts that have to name them, and the process boundaries are `__worker`
and `__uninstall_runner`. A person never types any of them: `install` resolves an
upgrade from the machine's own record, and an interrupted transaction is reconciled
from the record the engine wrote before it started.

See [architecture](docs/architecture.md) for the portable core, target lowering,
the platform backend boundary, the target matrix, the per-crate boundary, the
persisted format versions, and the leak gate.

Zup is a prototype: every internal format is version 1, and an incompatible
change is expected without a version bump.

## Installer frontends

See [GUI, console, and headless installer frontends](docs/frontends.md) for
build-time selection, automation output, exit codes, elevation, and Server Core
guidance.

## Plugins

See [plugin authoring and runtime architecture](docs/plugins.md) and the
[Rust configure example](examples/plugins/configure).

## Packages and Windows installer artifacts

`zup-bundle` writes and reads a portable schema-1 package. The file contains a
60-byte header, a SHA-256-protected JSON index for one target, and Zstandard
compressed content-addressed blobs. `Package::open` verifies the index and all
blobs before exposing payload, plugin, or prerequisite data. The same package can
be read on any host without an executable.

On Windows, `zup-windows` adapts that package to a PE: the package index is
stored in resource 1 and each compressed blob is stored in the following
resources. The adapter assigns those resource identifiers at embed time; they
are not part of the portable package schema. Sign the completed executable
after `zup build` so the signature covers the embedded package.

## Releasing with GitHub Actions

```yaml
permissions:
  contents: write

steps:
  - uses: actions/checkout@v7
  - uses: orielhaim/zup@action-v1
    with:
      operation: release
```

The action installs a verified zup CLI, runs the phase you asked for, and reports
the result as annotations, a job summary and typed outputs. The publishing token
is present only in the environment of `zup publish github` — never in a build,
which may run Tauri, Electron, Cargo build scripts and npm scripts.

For the whole pipeline instead of one step, `zup ci github generate` writes a
readable `plan → build → compose → attest → publish` workflow that calls the same
action once per phase. See [the zup GitHub Action](docs/action.md) for inputs,
outputs, the security model, permissions, attestation, workflow artifacts, GitHub
Enterprise, self-hosted runners, and the release/versioning model — and
[GitHub Releases as a distribution host](docs/github-distribution.md) for the
release format itself.

## Repository tasks

The package matrices and the GitHub Action refs are single-sourced in `zup-xtask`:

```text
cargo xtask emit-portable-matrix
cargo xtask verify-portable-boundaries
cargo xtask toolchain build
cargo xtask github-action-pins check
cargo xtask github-action-pins refresh
```

`verify-portable-boundaries` fails when a portable crate depends on a
Windows-only crate, declares a Windows-only platform table, names a Windows API,
branches on `cfg(windows)` in production code, reintroduces a
Windows-specific identifier, or spells a Windows concept inside a string
literal.

`toolchain build` is the step that produces the binaries a build composes an
installer from. It is also what keeps the launcher's dependency closure out of
`cargo test`: the four launcher images are built here, as their own step, and
staged beside `zup` with the descriptors the resolver checks.

`github-action-pins check` validates `github-actions.lock.json` — the ref each
third-party action uses, the commit it resolved to, and the date that was checked
— and fails when a committed workflow uses a `uses:` the lock does not track. It
is offline and runs in every CI job; `--online` additionally reports a newer major
series or a ref that no longer points where the lock recorded.
`cargo xtask github-action-pins refresh` reaches GitHub, advances the series and
re-records the commits. It never runs inside a build.

The action itself is developed in [`action/`](action) with Bun as the toolchain
and Node 24 as the runtime, with a committed bundle that CI proves matches its
source and runs under Node:

```bash
cd action
bun install --frozen-lockfile
bun run check          # typecheck, lint, test, bundle, metadata, Node 24 run
bun run check:dist     # the committed bundle is current
```

Bun installs, tests and bundles; `action/dist/index.js` runs on GitHub's Node 24
runtime. Nothing in `action/src` may use a `Bun.*` API — that is a compile error,
because the source's `tsconfig.json` declares `"types": ["node"]` and nothing
else.
