# Architecture

Zup compiles a declarative manifest into one portable package, then adapts
that package to a concrete machine. Every stage is platform-neutral until the
Windows adapter, and the boundary between them is checked by
`cargo xtask verify-portable-boundaries`.

## Pipeline

```text
zup.toml
  │  zup-manifest: select_targets          target resolution
  ▼
ResolvedTargetConfig (one per selected profile)
  │  zup-manifest: compile                 semantic IR
  ▼
zup_core::Installer                        normalized, target-bound
  │  zup-build: materialize                build inventory
  ▼
zup_build::BuildPlan / TargetBuildPlan     real files: size, SHA-256, destination
  │  zup-plan: plan                        semantic planning
  ▼
zup_plan::InstallPlan                      scope, components, resources, privileges
  │  backend: resolve_target               target lowering
  ▼
zup_platform::TargetPlan                   concrete target paths, no templates
  │  zup-exec: plan_execution              desired versus observed
  ▼
zup_exec::ExecutionPlan                    create / replace / no-op / conflict
  │  backend: plan_target_lifecycle        opaque backend transaction
  ▼
zup_transaction::TransactionPlan           graph, journal, receipts
  │  zup-runtime: run_install_control      runtime
  ▼
zup_runtime::InstallOutcome                commit, rollback, reboot, cancel
```

Each arrow is a typed hand-off. A stage cannot read the previous stage's
inputs from the filesystem, and no stage below the adapter imports a Windows
crate.

### Authoring

`zup-manifest` owns `zup.toml`: parsing, `schema` version validation, the JSON
schema, and the target matrix. It produces `zup_core::Installer`, the semantic
IR. The IR has its own explicit identity and is the only thing the engine
consumes, so the authoring syntax can evolve without touching the runtime.

A manifest is a top-level `schema = 1` plus a matrix of target profiles, and
every resource may be filtered by profile:

```toml
schema = 1
frontend = "gui"

[app]
id = "com.acme.desktop"
name = "Acme"
version = "1.4.0"
publisher = "Acme Inc."
main = "Acme.exe"

[build]

[build.targets.windows-x64]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist/x64" }

[build.targets.windows-arm64]
target = "aarch64-pc-windows-msvc"
source = { directory = "dist/arm64" }
frontend = "headless"

[install]
scope = "either"

[install.directory]
user = "${location.user_data}/Programs/Acme"
machine = "${location.programs}/Acme"

[[components]]
id = "core"
name = "Application"
required = true

[[components]]
id = "cli"
name = "Command-line tools"
requires = ["core"]
targets = ["windows-x64"]

[[files]]
source = "**/*"
destination = "${location.programs}/Acme"
component = "core"
targets = ["windows-x64"]

[[services]]
id = "acme-agent"
name = "Acme Agent"
binary = "${location.programs}/Acme/acme-agent.exe"
start = "automatic"
component = "core"
targets = ["windows-x64"]
```

`schema`, `[app]`, `[build]`, and `[install]` are required. `[build]` declares
`targets` with at least one profile; the empty `[build]` header is only needed
when profiles are written as `[build.targets.<profile>]` dotted keys. Every
`[[array]]` of resources is optional. The required shape above is the whole
authoring surface; the parser rejects every other key, so a manifest cannot
carry an option the engine does not implement.

### Target resolution and canonical target identity

A **target profile** is a friendly name; a **target triple** is the canonical
identity of a machine. `zup_core::TargetTriple` normalizes a triple through
`target-lexicon` (`x64-windows-msvc` becomes `x86_64-pc-windows-msvc`) and
rejects an unknown architecture or operating system, so two spellings of one
target can never diverge downstream. Two profiles that resolve to the same
canonical triple are a manifest error.

`--target` is repeatable and accepts either form. A profile name wins over a
triple when both match, and an empty selection means every profile. Results are
ordered by profile name, so a build is reproducible.

### Precedence

| Setting | Lowest | Highest |
| --- | --- | --- |
| Frontend | manifest `frontend` | profile `frontend`, then `--frontend` |
| Install scope and directory | manifest `[install]` | profile `install` |
| Resource applicability | every profile | `targets = [...]` on the resource |
| Runtime template | template discovered next to the executable | explicit `--runtime` |
| Output path | name derived from the app name | explicit `--output` |

`--runtime` and `--output` are positional against the selected targets: one of
each per target, in selection order. A single selected target may omit
`--runtime` and use the template found next to the `zup` executable. Missing,
duplicated, or miscounted inputs are reported per target, so one run shows
every problem instead of the first. `zup doctor` reports the same resolution
read-only, as readiness checks.

### Profile resource filters

Every resource declaration is a `Targeted<T>`: the value plus an optional
`targets` list. An empty list means every selected profile; a non-empty list
names profiles explicitly, and an unknown profile name is a validation error.
Filtering happens once, during compilation for the selected profile, so the IR
contains only resources that target will install.

### Build inventory

`zup-build` walks the source directory without following links, expands
`[[files]]` patterns, and resolves each destination to a `RelativePath` plus a
size and a SHA-256. An embedded prerequisite directory is rejected outright if
any parent is a reparse point or a symlink, so a package cannot smuggle a
trusted-looking path out of its source tree. The result is sorted and
platform-neutral: it describes a payload, not an installer.

### Planning

`zup-plan` answers one question: what should this installation contain for this
scope and component selection? It expands templates symbolically, selects
components, and assigns a `Privilege` to every resource. Scope says *where* an
application lives; `Privilege` says *how the host authorizes the work*, and
only defaults are shared.

Plugins join the plan as a `PluginExecutor`: a bounded WebAssembly component
returns typed resources and generated files, which are hashed and merged into
the ordinary plan. Generated files use the same transaction, ownership, repair,
and uninstall path as manifest files.

### Target lowering

Lowering converts a platform-neutral plan into paths on a concrete machine. It
resolves install locations and templates, produces `zup_platform::TargetPlan`,
and rejects invalid target paths and case-insensitive collisions. The output
contains no `Template` values and no build-machine paths, so it is a
description of a machine, not of a build.

A template resolves `${location.*}` against a semantic `InstallLocation` that
the backend maps for the selected scope: `programs`, `user_data`, `shared_data`,
`menu`, and `desktop`. `programs` resolves to the machine-wide program folder;
`user_data` and `shared_data` to the per-user and per-machine data folders;
`menu` and `desktop` to the scope's own menu folder and desktop. The remaining
template variables are `app.id`, `app.name`, `app.version`, and `install`, which
expands to the install directory template of the selected scope. A template that
leaves a variable unresolved at lowering time is an error, not an empty path.

### Opaque backend transaction

The portable engine cannot undo a registry write, a service, or a shell link,
so it does not pretend to. `zup_transaction` journals file operations with full
receipts and delegates platform state to the backend as an **opaque payload**
keyed by a `BackendResourceId`:

```text
OperationReceipt::Backend { key, payload }
```

The engine validates the payload's size, journals it, hands it back to the
adapter on verify, rollback, and reconcile, and never interprets it. Ownership
of a backend resource is therefore decided by the adapter that created it,
which is what makes ownership-aware uninstall possible.

### Runtime

`zup_runtime` owns the session: the event stream, cooperative cancellation,
the session log, recovery discovery, and the authorization policy. The
synchronous transaction engine runs under `spawn_blocking`.

## The target matrix and the backend boundary

`zup build` and `zup check` classify every selected target against the backends
this host implements, before the source tree is walked. The classification
reads nothing from disk, so an unsupported target never costs a payload
inventory or a prerequisite resolution first.

| Target | Build host | Result |
| --- | --- | --- |
| Windows triple | Windows | built |
| Windows triple | non-Windows | refused: `backend unavailable` |
| non-Windows triple | any | refused: `backend not implemented` |

`zup build` and `zup check` report the refusal as an `unsupported backend for
target` error naming the triple, with the reason from the table, and exit
nonzero. `zup doctor` reports the same refusal as its `backend` check and
continues with the remaining checks, so one run shows every problem; the
`lowering` check is skipped for a target with no backend.

There is no Linux backend. No crate in this repository lowers a plan to Linux
system integration. A portable target triple is accepted by the manifest, parsed,
and canonicalized, and then refused at the boundary.

## Command line

Authoring commands share the target selection and override rules above.

| Command | Purpose |
| --- | --- |
| `zup build` | Produce a self-contained installer executable |
| `zup init` | Write a small, editable `zup.toml` and its source directory |
| `zup check` | Validate a manifest and its build inputs |
| `zup doctor` | Report build readiness for the selected targets |
| `zup plan` | Print the real installation plan without touching the machine |
| `zup schema` | Print or write the authoritative JSON Schema |
| `zup fmt` | Format `zup.toml` while preserving comments |
| `zup completions <shell>` | Write shell completions to stdout |

`zup build`:

```text
--manifest <MANIFEST>          [default: zup.toml]
--output <OUTPUT>              repeatable
--runtime <RUNTIME>            repeatable
--source <SOURCE>              repeatable
--install-directory <PATH>     repeatable, alias --install-dir
--frontend <gui|console|headless>
--force
--target <PROFILE_OR_TARGET>   repeatable
```

`zup check` takes `--manifest`, `--source`, `--install-directory`, and
`--target`. `zup plan` takes `--manifest`, `--target`, `--scope`, `--state-root`,
`--enable`, `--disable`, `--install-directory`, and `--json`; it requires
exactly one target, so a multi-profile manifest must name one with `--target`.

`zup doctor` takes `--manifest`, `--runtime`, `--output`, `--source`,
`--install-directory`, `--frontend`, `--target`, and
`--format <human|json>`. It reports one row per check per profile, for the
canonical target, manifest compile, source payload, plugin engine, update root,
frontend, runtime template, runtime target, runtime subsystem, build backend,
target lowering, and output parent. The JSON report is versioned
(`version: 1`) and carries `profile`, `target`, `kind`, `status`, `message`,
and `path` per check, so a consumer reads `status` and `path` without parsing
prose.

`zup schema --output schema/zup.schema.json` regenerates the published JSON
Schema; CI fails when the checked-in file differs from what the code emits.

`zup build` embeds a runtime template executable, so a frontend runtime has to
exist before the build runs. The `zup` library compiles only with the `build`
feature today, which means a template is built with `build` enabled; see
[installer frontends](frontends.md).

Lifecycle commands (`install`, `upgrade`, `modify`, `repair`, `uninstall`,
`update`, `recover`) take `--output <human|json|jsonl>` and a small set of
scope, state, and component options. See
[installer frontends](frontends.md) for the output formats and exit codes.

## Portable crates and the Windows adapter

The portable stack is platform-neutral by construction. `zup-xtask` holds the
authoritative package matrices and the only classification of every workspace
member:

```text
cargo xtask emit-portable-matrix
```

| Matrix | Host | Contents |
| --- | --- | --- |
| `portable-core` | any | `zup-core`, `zup-manifest`, `zup-build`, `zup-plan`, `zup-platform`, `zup-exec`, `zup-transaction`, `zup-bootstrap`, `zup-bundle`, `zup-protocol`, `zup-runtime`, `zup-presentation`, `zup-update`, `zup-plugin-contract`, `zup-plugin-build`, `zup-plugin-runtime` |
| `portable-tests` | any | `zup-xtask` |
| `windows-only` | Windows | `zup-windows`, `zup`, `zup-ui` |

`zup` is the composition CLI. It is Windows-only because it links the Windows
adapter to answer `zup plan` and to run the lifecycle, not because the manifest
model or the build pipeline needs a Windows host.

`cargo xtask verify-portable-boundaries` fails when a portable crate:

- depends on `windows`, `winapi`, or `zup-windows` in any manifest table;
- declares a `[target.'cfg(windows)']` table;
- imports `std::os::windows` in production source;
- names `windows::Win32`, `Win32::`, `winapi::`, or `windows_bindgen`;
- branches on `cfg(windows)`, `cfg(not(windows))`, or a Windows `target_os` or
  `target_family` predicate in production source;
- reintroduces a Windows-specific identifier such as `Registry`, `CLSID`,
  `ServiceControlManager`, `OpenSCManager`, `KnownFolder`, or `UninstallEntry`.

String literals are a separate rule and a separate pass: a concept spelled in a
literal is matched in its decoded form, case-insensitively, so `registry` in a
message string is a finding. Windows is allowed in `zup-windows`, in the
product frontends and binaries, in tests and documentation, and inside
target-lexicon identifiers such as `TargetOperatingSystem::Windows`. The check
reads files only, so it returns the same answer on every host. Comments are
blanked before matching and `#[cfg(test)]` blocks are skipped, so prose and test
fixtures do not produce findings. Findings are ordered by matrix, package,
path, and line.

The check also requires the matrices and the workspace to agree: every member
is classified, every matrix package exists, and no package is in two matrices.
Adding a crate means adding it to exactly one matrix.

## Package and executable

`zup-bundle` writes a portable schema-1 package: a 60-byte header, a
SHA-256-protected JSON index for one target, and Zstandard-compressed
content-addressed blobs. `Package::open` verifies the index and every blob
before exposing payload, plugin, or prerequisite data. The same bytes are
readable on any host without an executable.

`zup-windows` adapts that package to a PE. The index becomes resource 1 and
each compressed blob becomes the following resource; identifiers are assigned
at embed time and are not part of the package schema. `zup build` also rejects
a runtime whose PE subsystem or template name does not match the selected
frontend, and refuses to embed into an already signed executable. Sign the
finished artifact, because Authenticode covers the embedded package.

## Runtime backend seam

The runtime never names a platform backend. It depends on one trait:

```rust
pub trait RuntimeBackend: Send + Sync {
    fn payload_source(&self, request: &RuntimeRequest) -> RuntimePayloadSource;
    fn execute<'a>(&'a self, request: RuntimeRequest, control: RuntimeControl)
        -> RuntimeFuture<'a, Result<InstallOutcome, SessionError>>;
}
```

`WindowsRuntimeBackend` is the only implementation: it supplies a payload
source over the embedded package and the temporary payload overlay, then runs
the transaction coordinator, the Restart Manager preflight, and the
authenticated worker. A second backend implements the same trait and nothing
else.

## Target binding

Canonical target identity is checked wherever a target crosses a trust or
persistence boundary.

- **Bootstrap.** A `BoundBootstrapPlan` carries the target it was planned for.
  `run_install_control` refuses a request whose target differs from the
  transaction plan or from the bootstrap plan.
- **Plugins.** A component is compiled ahead of time for the build host triple
  (`ZUP_BUILD_TARGET`) and embedded with that target, the WIT digest, the
  engine fingerprint, and its AOT format version. The loader requires the
  package target, the requested target, and the host compile target to be the
  same triple, and refuses an artifact whose fingerprint, API version, or AOT
  format does not match. A component built for another target cannot be
  loaded.
- **Protocol.** `zup-protocol` frames carry `PROTOCOL_VERSION`; a mismatch
  fails the handshake instead of misreading a frame. The parent/worker pair
  also verifies process identity and the plan hash before executing.

## Persisted formats

Zup is a prototype: every internal format is version 1, and an incompatible
change is expected without a version bump.

| Persisted form | Constant | Version |
| --- | --- | --- |
| `zup.toml` `schema` | `zup_manifest::SCHEMA_VERSION` | 1 |
| Package index and blobs | `zup_bundle::PACKAGE_SCHEMA` | 1 |
| Process protocol frame | `zup_protocol::PROTOCOL_VERSION` | 1 |
| Plugin API | `PLUGIN_API_VERSION` | 1.0.0, AOT format 1, Wasmtime 49.0.0 |
| Install ledger | `zup_exec::INSTALL_LEDGER_SCHEMA` | 1 |
| Transaction plan | `TRANSACTION_PLAN_SCHEMA` | 1 |
| Journal record | `JOURNAL_SCHEMA` | 1 |
| Bootstrap plan and state | `BOOTSTRAP_PLAN_SCHEMA`, `BOOTSTRAP_STATE_SCHEMA` | 1 |
| Automation protocol | `zup_presentation::AUTOMATION_PROTOCOL_VERSION` | 1 |
| Doctor report | `zup::doctor::REPORT_VERSION` | 1 |

Each loader compares the stored version before deserializing, and a mismatch is
an error.

## Continuous integration

Two jobs: `windows` on `windows-latest` and `ubuntu (portable)` on
`ubuntu-latest`. Both run with `contents: read` and
`persist-credentials: false`, and neither writes outside the runner's build
cache.

The Windows job runs, in order: `cargo fmt --all --check`, `cargo clippy
--workspace --all-features --all-targets -- -D warnings`, `cargo nextest run
--workspace --all-features`, `cargo test --workspace --all-features --doc`,
`cargo machete`, `git diff --check`, `cargo xtask verify-portable-boundaries`,
a `zup schema --output schema/zup.schema.json` step that fails when the
checked-in schema differs from the generated one, and
`scripts/verify-frontend-features.ps1`, which proves the three frontend
templates build with the intended dependency graphs and PE subsystems.

The Ubuntu job emits both portable matrices, then verifies the portable stack
natively on Linux:

```text
cargo xtask emit-portable-matrix
cargo xtask verify-portable-boundaries
cargo check   --all-features $PORTABLE
cargo nextest run --all-features $PORTABLE
cargo test --doc --all-features $PORTABLE
cargo clippy  --all-features --all-targets $PORTABLE -- -D warnings
cargo machete
git diff --check
```

`$PORTABLE` is the `-p` argument list of `portable-core` plus `portable-tests`.

The Linux job proves one thing: the portable stack compiles and its tests pass
natively on a non-Windows host. It is not evidence of a Linux backend, because
no crate lowers a plan to Linux system integration. A manifest naming a
non-Windows target is refused at the boundary on any host.

## Current status

The Windows backend is implemented: `zup build` produces a self-contained
installer for every Windows target, and the runtime installs, updates, repairs,
and uninstalls through the transaction engine and the authenticated worker.

The portable stack is not a claim about Linux support. It is the property that
the semantic model, the build inventory, planning, the transaction engine, the
package format, plugins, the runtime session, and the protocol build and test
natively on Linux today, in CI, so the Windows adapter stays an adapter.
