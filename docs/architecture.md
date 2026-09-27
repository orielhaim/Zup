# Architecture

Zup compiles a declarative manifest into one portable package, composes that
package into a distribution artifact, then adapts the artifact to a concrete
machine. Every stage is platform-neutral until the Windows adapter, and the
boundary between them is checked by
`cargo xtask verify-portable-boundaries`.

The measurements and the decisions behind the output side are in
[the artifact graph report](artifact-graph.md).

## Pipeline

The output side is one chain, and each link is a distinct type:

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
zup_build::TargetBuildPlan                 real files: size, SHA-256, destination
  │  zup-artifact: DistributionVariant::resolve
  ▼
DistributionVariant                        one resolved native target: content,
                                          runtime, requirements, trust
  │  zup-artifact: ArtifactComposer        shared store, index, variant manifests
  ▼
ArtifactGraph                              the content-addressed descriptor graph
  │  backend: compose_universal_executable
  ▼
DistributionArtifact                       the file a user downloads
```

A `DistributionVariant` is one machine's worth of work, built locally, cached and
executable on its own; a `DistributionArtifact` is one file, possibly carrying
several variants, composed after every variant it needs exists. They are not
interchangeable, and keeping them apart is what lets variants be built in parallel
on different machines and artifacts composed wherever the outputs meet.

On the install side the chain continues:

```text
DistributionArtifact
  │  backend: stage_variant                selected variant only
  ▼
Portable package                           schema 1, one target
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

`docs/artifact-graph.md` owns the artifact graph itself: the four rules its
readers enforce, how a variant is selected, the resource layout of the universal
Windows artifact, the launcher, and what a machine keeps after an install.

## Authoring

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

[build.artifacts.windows]
kind = "universal"
mode = "offline"
targets = ["windows-x64", "windows-arm64"]
channel = "stable"
output = "Acme-Windows-Setup.exe"

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

`[build.artifacts.<id>]` is optional and declares a file this project
publishes:

| Key | Meaning |
| --- | --- |
| `kind` | `universal` (all listed targets) or `single` (exactly one) |
| `mode` | `offline` (content in the file) or `thin` (content fetched) |
| `targets` | Profiles to include. Empty means every selected target. |
| `channel` | Release channel this artifact follows. Absent means an exact version. |
| `output` | File name. Absent derives one from the app name. |

A project that declares no artifacts builds one installer per selected target.
Declaring artifacts changes what `zup build` produces, nothing else.

Every resource declaration is a `Targeted<T>`: the value plus an optional
`targets` list. An empty list means every selected profile; a non-empty list names
profiles explicitly, and an unknown profile name is a validation error. Filtering
happens once, during compilation for the selected profile, so the IR contains
only resources that target will install.

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
each per target, in the manifest's own target order, which is ordered by profile
name. A miscounted flag names the profiles in that order rather than leaving the
reader to guess which template goes with which target. A single selected target
may omit `--runtime` and use the template found next to the `zup` executable.
Missing, duplicated, or miscounted inputs are reported per target, so one run
shows every problem instead of the first. `zup doctor` reports the same
resolution read-only, as readiness checks.

## From inventory to transaction

`zup-build` walks the source directory without following links, expands
`[[files]]` patterns, and resolves each destination to a `RelativePath` plus a
size and a SHA-256. An embedded prerequisite directory is rejected outright if
any parent is a reparse point or a symlink, so a package cannot smuggle a
trusted-looking path out of its source tree. The result is sorted and
platform-neutral: it describes a payload, not an installer.

`zup-plan` answers one question: what should this installation contain for this
scope and component selection? It expands templates symbolically, selects
components, and assigns a `Privilege` to every resource. Scope says *where* an
application lives; `Privilege` says *how the host authorizes the work*, and only
defaults are shared. Plugins join the plan as a `PluginExecutor`: a bounded
WebAssembly component returns typed resources and generated files, which are
hashed and merged into the ordinary plan, and generated files use the same
transaction, ownership, repair, and uninstall path as manifest files.

Lowering converts that platform-neutral plan into paths on a concrete machine. It
resolves install locations and templates, produces `zup_platform::TargetPlan`,
and rejects invalid target paths and case-insensitive collisions. The output
contains no `Template` values and no build-machine paths, so it is a description
of a machine, not of a build.

A template resolves `${location.*}` against a semantic `InstallLocation` that
the backend maps for the selected scope: `programs`, `user_data`, `shared_data`,
`menu`, and `desktop`. `programs` resolves to the machine-wide program folder;
`user_data` and `shared_data` to the per-user and per-machine data folders;
`menu` and `desktop` to the scope's own menu folder and desktop. The remaining
template variables are `app.id`, `app.name`, `app.version`, and `install`, which
expands to the install directory template of the selected scope. A template that
leaves a variable unresolved at lowering time is an error, not an empty path.

The portable engine cannot undo a registry write, a service, or a shell link, so
it does not pretend to. `zup_transaction` journals file operations with full
receipts and delegates platform state to the backend as an **opaque payload**
keyed by a `BackendResourceId`:

```text
OperationReceipt::Backend { key, payload }
```

The engine validates the payload's size, journals it, hands it back to the
adapter on verify, rollback, and reconcile, and never interprets it. Ownership of
a backend resource is therefore decided by the adapter that created it, which is
what makes ownership-aware uninstall possible.

`zup_runtime` owns the session: the event stream, cooperative cancellation, the
session log, recovery discovery, and the authorization policy. The synchronous
transaction engine runs under `spawn_blocking`.

### Runtime backend seam

The runtime never names a platform backend. It depends on one trait:

```rust
pub trait RuntimeBackend: Send + Sync {
    fn payload_source(&self, request: &RuntimeRequest) -> RuntimePayloadSource;
    fn execute<'a>(&'a self, request: RuntimeRequest, control: RuntimeControl)
        -> RuntimeFuture<'a, Result<InstallOutcome, SessionError>>;
}
```

`WindowsRuntimeBackend` is the only implementation: it supplies a payload source
over the embedded package and the temporary payload overlay, then runs the
transaction coordinator, the Restart Manager preflight, and the authenticated
worker. A second backend implements the same trait and nothing else.

### Target binding

Canonical target identity is checked wherever a target crosses a trust or
persistence boundary.

- **Bootstrap.** A `BoundBootstrapPlan` carries the target it was planned for.
  `run_install_control` refuses a request whose target differs from the
  transaction plan or from the bootstrap plan.
- **Plugins.** A component is compiled ahead of time for the build host triple
  (`ZUP_BUILD_TARGET`) and embedded with that target, the WIT digest, the engine
  fingerprint, and its AOT format version. The loader requires the package
  target, the requested target, and the host compile target to be the same
  triple, and refuses an artifact whose fingerprint, API version, or AOT format
  does not match. A component built for another target cannot be loaded.
- **Protocol.** `zup-protocol` frames carry `PROTOCOL_VERSION`; a mismatch fails
  the handshake instead of misreading a frame. The parent/worker pair also
  verifies process identity and the plan hash before executing.

## The target matrix and the backend boundary

`zup build` and `zup check` classify every selected target against the backends
this host implements, before the source tree is walked. The classification reads
nothing from disk, so an unsupported target never costs a payload inventory or a
prerequisite resolution first.

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
system integration: a portable target triple is accepted by the manifest, parsed,
and canonicalized, and then refused at the boundary.

## Command line

Authoring commands share the target selection and override rules above.

| Command | Purpose |
| --- | --- |
| `zup build` | Produce installers or composed distribution artifacts |
| `zup artifact inspect` | Describe what a built artifact contains and how it verifies |
| `zup init` | Write a small, editable `zup.toml` and its source directory |
| `zup check` | Validate a manifest and its build inputs |
| `zup doctor` | Report build readiness for the selected targets |
| `zup plan` | Print the real installation plan without touching the machine |
| `zup publish stage` | Write the web tree a static origin serves, and the transport packages a release host holds |
| `zup publish github` | Publish a release to GitHub Releases |
| `zup ci github generate` | Write `.github/workflows/release.yml` from the manifest |
| `zup ci github check` | Report whether the committed workflow matches the generator |
| `zup schema` | Print or write the authoritative JSON Schema |
| `zup fmt` | Format `zup.toml` while preserving comments |
| `zup completions <shell>` | Write shell completions to stdout |

`zup build`:

```text
--manifest <MANIFEST>          [default: zup.toml]
--output <OUTPUT>              repeatable
--runtime <RUNTIME>            repeatable, hidden
--source <SOURCE>              repeatable
--install-directory <PATH>     repeatable, alias --install-dir
--frontend <gui|console|headless>
--force
--target <PROFILE_OR_TARGET>   repeatable
--artifact <ARTIFACT>          repeatable, conflicts with --universal
--universal                    compose every selected target into one file
--dispatcher <PATH>            launcher component, hidden; resolved when absent
--release-manifest <PATH>      release description, or `none`; [default: dist/zup-release.json]
```

`--target` and `--artifact` answer different questions and are not synonyms.
`--target` names a native variant to build or debug, and produces one ordinary
installer per named target. `--artifact` names a file a user downloads, and
produces the composed graph. `--universal` is the shorthand for "compose every
selected target into one file" and is refused alongside `--artifact`.

`--dispatcher` names a launcher component to compose into instead of the one
resolution finds. The component must be unsigned, must present the launcher
experience the artifact's variants agreed on, and must be no wider than the
narrowest machine among them. When the flag is absent, the toolchain resolver
finds the launcher the same way it finds a runtime template.

The runtime template and the launcher are zup's own binaries, so asking every
project author to compile them before their first build is a tax on the ordinary
path to an installer. Instead the build asks for a semantic component and the
resolver finds the bytes: an explicit `--runtime` or `--dispatcher` path, then an
explicit toolchain root from `--toolchain` or `ZUP_TOOLCHAIN`, then the installed
cache for this exact zup version, then a toolchain staged beside the executable.
There is no network step, and a resolved component is checked against its
descriptor and its own PE header before it is used, so a template from another
zup release, machine, or presentation is refused rather than embedded.

```text
cargo xtask toolchain build [--profile dev|release]
```

builds the three runtime templates and four launchers, writes a
`.zup-toolchain.json` descriptor beside each, and stages them in
`target/<profile>/toolchain/<version>/`. The default profile is `dev`.

`zup artifact inspect <ARTIFACT> [--format <human|json>]` reads a composed
artifact with the same parser the launcher and the runtime use, and verifies
every content digest it reports. The JSON report is versioned
(`report_version: 1`) and states the artifact's kind, mode, pin, launcher
subsystem, its variants, what composition cost and saved, and what could be
proven about trust: Authenticode presence, index validity, content digests, and
variant completeness.

`zup check` takes `--manifest`, `--source`, `--install-directory`, and
`--target`. `zup plan` takes `--manifest`, `--target`, `--source`,
`--install-directory` (alias `--install-dir`), `--frontend`, `--scope`,
`--state-root`, `--enable`, `--disable`, and `--json`; it requires exactly one
target, so a multi-profile manifest must name one with `--target`.

`zup doctor` takes `--manifest`, `--runtime`, `--output`, `--source`,
`--install-directory`, `--frontend`, `--target`, and `--format <human|json>`. It
resolves runtimes through the toolchain resolver and reports one row per check
per profile: `target`, `compile`, `payload`, `plugins`, `updates`, `frontend`,
`runtime`, `backend`, `lowering`, and `output`. The `runtime` row is one check
because a build refuses a component that fails any part of it, and it names the
source the component was resolved from. The JSON report is versioned
(`version: 1`) and carries `profile`, `target`, `kind`, `status`, `message`, and
`path` per check, so a consumer reads `status` and `path` without parsing prose.

`zup schema --output schema/zup.schema.json` regenerates the published JSON
Schema; CI fails when the checked-in file differs from what the code emits.

`zup publish stage` writes what a static origin serves and, with `--packages`,
the transport packages a release host holds. `zup publish github` and
`zup ci github generate` / `zup ci github check` are documented in
[GitHub distribution](github-distribution.md).

The installer runtime's public verbs are `install`, `modify`, `repair`, `update`,
and `uninstall`. They take `--output <human|json|jsonl>` and a small set of
scope, state, and component options, and they live in `zup-installer`, not in
`zup`. `upgrade` and `recover` exist as hidden `__upgrade` and `__recover`, and
`__worker` and `__uninstall_runner` are the process boundaries a parent spawns.
See [installer frontends](frontends.md) for the output formats and exit codes.

### The developer CLI and the installer runtime are separate packages

They are two packages because a role must be a package and not a Cargo feature. A
feature is a compile-time switch inside one binary; a role is a different product
with a different dependency graph, a different lifecycle, and a different
consumer. `zup` is the developer's authoring and distribution surface, and it has
no Cargo features at all, so `cargo run -p zup -- --help` and
`cargo install --path crates/zup` need no flag to produce the CLI. The only
features in the repository belong to the runtime in `zup-installer`: `gui`,
`console`, and `headless`, which produce `zup-setup-gui`, `zup-setup-console`, and
`zup-setup-headless`.

The boundary is the point, so it is checked rather than asserted.
`scripts/verify-frontend-features.ps1` proves each runtime frontend builds alone,
that no Cargo feature selects a role, and that `zup-installer` cannot reach
`zup-build`, `zup-manifest`, `zup-plugin-build`, `zup-publish`, `zup-xtask`, or
`zup` at all. It also proves `zup` is the only installable binary: every other
package is `publish = false`. The presentations themselves are in
[installer frontends](frontends.md).

## Portable crates and the Windows adapter

The portable stack is platform-neutral by construction. `zup-xtask` holds the
authoritative package matrices and the only classification of every workspace
member:

```text
cargo xtask emit-portable-matrix
```

| Matrix | Host | Contents |
| --- | --- | --- |
| `portable-core` | any | `zup-core`, `zup-manifest`, `zup-build`, `zup-plan`, `zup-platform`, `zup-exec`, `zup-transaction`, `zup-bootstrap`, `zup-bundle`, `zup-artifact`, `zup-protocol`, `zup-runtime`, `zup-presentation`, `zup-update`, `zup-plugin-contract`, `zup-plugin-build`, `zup-plugin-runtime` |
| `portable-tests` | any | `zup-xtask` |
| `windows-only` | Windows | `zup-pe`, `zup-windows`, `zup-dispatch`, `zup`, `zup-installer`, `zup-ui` |

`zup` and `zup-installer` are Windows-only because they link the Windows adapter
to answer `zup plan` and to run the lifecycle, not because the manifest model or
the build pipeline needs a Windows host. `zup-artifact` and `zup-pe` are on the
opposite sides of that line from each other, which is the point: the artifact
model is portable and the PE primitives are not.

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
message string is a finding. Windows is allowed in `zup-windows`, in the product
frontends and binaries, in tests and documentation, and inside target-lexicon
identifiers such as `TargetOperatingSystem::Windows`. The check reads files only,
so it returns the same answer on every host. Comments are blanked before matching
and `#[cfg(test)]` blocks are skipped, so prose and test fixtures do not produce
findings. Findings are ordered by matrix, package, path, and line.

The check also requires the matrices and the workspace to agree: every member is
classified, every matrix package exists, and no package is in two matrices. Adding
a crate means adding it to exactly one matrix.

## Package and executable

`zup-bundle` writes a portable schema-1 package: a 60-byte header, a
SHA-256-protected JSON index for one target, and Zstandard-compressed
content-addressed blobs. `Package::open` verifies the index and every blob before
exposing payload, plugin, or prerequisite data. The same bytes are readable on any
host without an executable. `BundleWriter::write_plan` writes a package from an
already-compressed blob set, which is how one variant is materialized out of a
shared multi-gigabyte store without decompressing and recompressing anything.

`zup-windows` adapts that package to a PE. The index becomes resource 1 and
each compressed blob becomes the following resource; identifiers are assigned at
embed time and are not part of the package schema. `zup-pe` holds the PE header
and resource primitives the launcher and the composer share, so there is one
implementation of the certificate table and the resource directory, and
`zup-windows::bundle_packager` delegates to it. `zup build` also rejects a runtime
whose PE subsystem or template name does not match the selected frontend, and
refuses to embed into an already signed executable. Sign the finished artifact,
because Authenticode covers the embedded package.

`AutoPayloadSource` reads its payload from one of two places: the package
embedded in the running executable, or a `variant.zup` sidecar beside a bare
`Setup.exe`. The sidecar is how a native runtime finds its content after the
launcher has staged it.

`stage_variant` materializes one selected variant into a per-user, SID-bound
content store and writes three files: the variant's native runtime, its package,
and the artifact index. It creates files and returns the digests it proved; it
opens nothing privileged, reads no registry, and touches no lifecycle state. The
store is per-user and never machine-wide, because the launcher that writes it
has no elevation and must not need any. Its directories are verified to be real
directories and not reparse points before anything is written through them, and
each records the digest of its own identity so a substituted directory is detected
rather than trusted. What a machine keeps afterwards is in
[the artifact graph report](artifact-graph.md).

## Persisted formats

Zup is a prototype: every internal format is version 1, and an incompatible
change is expected without a version bump.

| Persisted form | Constant | Version |
| --- | --- | --- |
| `zup.toml` `schema` | `zup_manifest::SCHEMA_VERSION` | 1 |
| Package index and blobs | `zup_bundle::PACKAGE_SCHEMA` | 1 |
| Artifact index and variant manifests | `ARTIFACT_INDEX_SCHEMA` | 1 |
| Content store table | `BLOB_TABLE_SCHEMA` | 1 |
| Release description | `RELEASE_SCHEMA` | 1 |
| Process protocol frame | `zup_protocol::PROTOCOL_VERSION` | 1 |
| Plugin API | `PLUGIN_API_VERSION` | 1.0.0, AOT format 1, Wasmtime 49.0.0 |
| Install ledger | `zup_exec::INSTALL_LEDGER_SCHEMA` | 1 |
| Transaction plan | `TRANSACTION_PLAN_SCHEMA` | 1 |
| Journal record | `JOURNAL_SCHEMA` | 1 |
| Bootstrap plan and state | `BOOTSTRAP_PLAN_SCHEMA`, `BOOTSTRAP_STATE_SCHEMA` | 1 |
| Automation protocol | `zup_presentation::AUTOMATION_PROTOCOL_VERSION` | 1 |
| Doctor report | `zup::doctor::REPORT_VERSION` | 1 |
| Artifact inspection report | `zup::inspect_artifact::REPORT_VERSION` | 1 |

Each loader compares the stored version before deserializing, and a mismatch is
an error.

## Continuous integration

Three workflows, all with `contents: read` and `persist-credentials: false`.

`ci.yml` has two jobs: `windows` on `windows-latest` and `ubuntu (portable)` on
`ubuntu-latest`. Neither writes outside the runner's build cache.

The Windows job runs, in order: `cargo fmt --all --check`, `cargo clippy
--workspace --all-features --all-targets -- -D warnings`,
`cargo xtask toolchain build`, `cargo nextest run --workspace --all-features`,
`cargo test --workspace --all-features --doc`, `cargo machete`,
`git diff --check`, `cargo xtask verify-portable-boundaries`,
`cargo xtask github-action-pins check`, a
`zup schema --output schema/zup.schema.json` step that fails when the
checked-in schema differs from the generated one, and
`scripts/verify-frontend-features.ps1`, which proves the three frontend
templates build with the intended dependency graphs and PE subsystems, that `zup`
declares no Cargo features, and that `zup` is the only installable binary.

The toolchain is a required input to the composition tests, not something
`cargo test` builds: it is a separate package with a deliberately small
dependency closure, which is the wrong trade for a 128 MB installer and the right
one for a launcher whose size is a design constraint. A test that needs a
launcher says so rather than passing without composing anything.

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
natively on a non-Windows host. It is not evidence of a Linux backend, because no
crate lowers a plan to Linux system integration, and a manifest naming a
non-Windows target is refused at the boundary on any host.

`action.yml` covers the GitHub Action itself, including the pin-lock gate, the
Bun/Node toolchain split, and the three jobs it runs; see
[the zup GitHub Action](action.md).

## Current status

The Windows backend is implemented: `zup build` produces a self-contained
installer for every Windows target, or one universal offline installer carrying
several of them, and the runtime installs, updates, repairs, and uninstalls
through the transaction engine and the authenticated worker.

Composition is complete for the offline mode. A thin artifact - one that carries
the index and a launcher and fetches content - is not yet produced;
`[build.artifacts]` accepts `mode = "thin"` and a `channel`, and composition
records both, but nothing fills a thin store yet. `zup-update` still resolves
updates from the release description's variant list rather than from a
content-addressed fetch. `docs/online-acquisition.md` has the acquisition design
and its measurements.

The portable stack is not a claim about Linux support. It is the property that
the semantic model, the build inventory, the artifact graph, planning, the
transaction engine, the package format, plugins, the runtime session, and the
protocol build and test natively on Linux today, in CI, so the Windows adapter
stays an adapter.
