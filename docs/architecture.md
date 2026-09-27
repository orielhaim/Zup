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

`DistributionVariant` and `DistributionArtifact` are different things and are
not interchangeable. A variant is one machine's worth of work, built locally,
cached, and executable on its own. An artifact is one file, possibly carrying
several variants, composed after every variant it needs exists. Keeping them
apart is what lets variants be built in parallel on different machines and
artifacts be composed wherever the outputs meet.

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

### The artifact graph

`zup-artifact` owns the whole model and is portable. A graph is a
content-addressed descriptor set, modelled on OCI's, with four rules:

- **Deterministic.** Blobs are packed into a table in ascending digest order and
  split into fixed-size segments, so the same content always produces the same
  table and the same byte layout. Canonical JSON is the only accepted encoding;
  a document that is merely *valid* JSON is refused.
- **Bounds-checked.** Every index, table, manifest, and metadata document has a
  declared size limit, and a reader allocates from a declared count rather than
  from a length field in the data. A corrupt or hostile artifact cannot make a
  reader allocate.
- **Content-addressed.** Every descriptor names a SHA-256 digest and a size.
  A reader verifies a blob before exposing it, so a store cannot be laundered
  into content that merely happens to parse.
- **Forward-versioned and fail-closed.** An index carries a required-feature
  bitmask. A reader that does not understand a required feature refuses the
  artifact rather than installing a subset of it.

Selection is driven by the index, never by a file name. `zup-artifact::select`
scores every candidate the index names and returns a typed answer — a
`CandidateVariant`, a `Compatibility`, a `SelectionScore` — rather than a
boolean. Native beats emulated; a tie is a refusal, not a coin flip; and a
variant that requires a machine component the host lacks is not offered as an
emulated fallback, because emulation cannot provide it.

OCI is used as an *adapter* only. `zup-artifact::oci` maps the same graph onto
`oci-spec`'s index and manifest types, and `export_oci_layout` writes a local
`oci-layout` directory for a consumer that already speaks OCI. Nothing in the
installer path depends on it, and no OCI client library is linked.

### The universal Windows artifact

`zup-windows` turns a composed graph into one PE file:

```text
Acme-Windows-Setup.exe
    dispatcher              a launcher, not an installer
    resource 1              the artifact index
    resource 2              the content store table
    resources 3..           one variant manifest per variant
    resources ..            one native runtime per variant
    resources ..            the content store, one region per segment
```

The layout is fixed and total, and `UniversalLayout` owns the identifier
arithmetic, so a reader knows which identifier holds what without consulting
the index first. Everything lives inside Authenticode-hashed image sections,
because resources are written before the image is signed; there is no trailing
overlay, so a signature covers the whole artifact.

The dispatcher is a launcher and nothing else. It inspects the host, validates
the index, selects a variant, verifies and materializes it into a per-user,
SID-bound content store, starts the variant's own native runtime, and forwards
its exit code. It holds no lifecycle authority: no registry, no services, no
elevation, no prerequisites. Every privileged operation happens inside the
selected native runtime, in its own architecture, which is what lets the
dispatcher be small enough to run under an emulation layer on a machine whose
native variant is something else.

`zup-pe` holds the PE header and resource primitives the dispatcher and the
composer share, so there is one implementation of the certificate table and the
resource directory. `zup-windows::bundle_packager` delegates to it.

The dispatcher is built for **32-bit x86**. Windows runs 32-bit x86 everywhere,
and 64-bit only where the operating system is 64-bit, so the narrowest variant
in an artifact decides how wide a dispatcher may be: a host that can run the
narrowest variant must be able to start the dispatcher first. Composition
enforces this rather than trusting the build step, and refuses a dispatcher
wider than the narrowest included variant. `scripts/build-dispatcher.ps1`
produces the images and installs them beside the `zup` executable, which is
where `zup build` and the composition tests look for a template.

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

A project that declares no artifacts builds one installer per selected target,
which is the simplest thing that works and the behaviour before artifacts
existed. Declaring artifacts changes what `zup build` produces, nothing else.

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
--runtime <RUNTIME>            repeatable
--source <SOURCE>              repeatable
--install-directory <PATH>     repeatable, alias --install-dir
--frontend <gui|console|headless>
--force
--target <PROFILE_OR_TARGET>   repeatable
--artifact <ARTIFACT>          repeatable, conflicts with --universal
--universal                    compose every selected target into one file
--dispatcher <PATH>            the launcher an artifact is composed into
--release-manifest <PATH>      release description, or `none`; [default: dist/zup-release.json]
```

`--target` and `--artifact` answer different questions and are not synonyms.
`--target` names a native variant to build or debug, and produces one ordinary
installer per named target. `--artifact` names a file a user downloads, and
produces the composed graph. `--universal` is the shorthand for "compose every
selected target into one file" and is refused alongside `--artifact`.

`--dispatcher` names the launcher template a composed artifact is written into.
The template must be unsigned, must present the launcher experience the
artifact's variants agreed on, and must be no wider than the narrowest machine
among them. When the flag is absent, `zup build` looks for `zup-dispatch.exe`
or `zup-dispatch-console.exe` beside itself.

`zup artifact inspect <ARTIFACT> [--format <human|json>]` reads a composed
artifact with the same parser the dispatcher and the runtime use, and verifies
every content digest it reports. The JSON report is versioned
(`report_version: 1`) and states the artifact's kind, mode, pin, launcher
subsystem, its variants, what composition cost and saved, and what could be
proven about trust: Authenticode presence, index validity, content digests, and
variant completeness.

`zup check` takes `--manifest`, `--source`, `--install-directory`, and
`--target`. `zup plan` takes `--manifest`, `--target`, `--scope`, `--state-root`,
`--enable`, `--disable`, `--install-directory`, and `--json`; it requires
exactly one target, so a multi-profile manifest must name one with `--target`.

`zup doctor` takes `--manifest`, `--runtime`, `--output`, `--source`,
`--install-directory`, `--frontend`, `--target`, and
`--format <human|json>`. It reports one row per check per profile, for the
canonical target, manifest compile, source payload, plugin engine, update root,
frontend, runtime template, runtime target, runtime subsystem, build backend,
target lowering, output parent, and composition. The composition row reports
what the selected targets would cost as separate installers and what one
composed artifact would store once, so the value of a universal artifact is
visible before anyone builds one. The JSON report is versioned
(`version: 1`) and carries `profile`, `target`, `kind`, `status`, `message`,
and `path` per check, so a consumer reads `status` and `path` without parsing
prose.

`zup schema --output schema/zup.schema.json` regenerates the published JSON
Schema; CI fails when the checked-in file differs from what the code emits.

`zup publish stage` writes what a static origin serves and, with `--packages`,
the transport packages a release host holds: one per variant, sharded only at a
fixed boundary and only when a package would exceed the host's per-asset limit.
`zup publish github` derives everything — the repository, the tag, the asset
list, the digests — and refuses rather than guessing when a repository cannot be
found. `zup ci github generate` and `zup ci github check` write and verify the
committed release workflow. All four are documented in
[GitHub distribution](github-distribution.md).

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
| `portable-core` | any | `zup-core`, `zup-manifest`, `zup-build`, `zup-plan`, `zup-platform`, `zup-exec`, `zup-transaction`, `zup-bootstrap`, `zup-bundle`, `zup-artifact`, `zup-protocol`, `zup-runtime`, `zup-presentation`, `zup-update`, `zup-plugin-contract`, `zup-plugin-build`, `zup-plugin-runtime` |
| `portable-tests` | any | `zup-xtask` |
| `windows-only` | Windows | `zup-pe`, `zup-windows`, `zup-dispatch`, `zup`, `zup-ui` |

`zup` is the composition CLI. It is Windows-only because it links the Windows
adapter to answer `zup plan` and to run the lifecycle, not because the manifest
model or the build pipeline needs a Windows host. `zup-artifact` and `zup-pe`
are on the opposite sides of that line from each other, which is the point:
the artifact model is portable and the PE primitives are not.

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
readable on any host without an executable. `BundleWriter::write_plan` writes a
package from an already-compressed blob set, which is how one variant is
materialized out of a shared multi-gigabyte store without decompressing and
recompressing anything.

`zup-windows` adapts that package to a PE. The index becomes resource 1 and
each compressed blob becomes the following resource; identifiers are assigned
at embed time and are not part of the package schema. `zup build` also rejects
a runtime whose PE subsystem or template name does not match the selected
frontend, and refuses to embed into an already signed executable. Sign the
finished artifact, because Authenticode covers the embedded package.

`AutoPayloadSource` reads its payload from one of two places: the package
embedded in the running executable, or a `variant.zup` sidecar beside a bare
`Setup.exe`. The sidecar is how a native runtime finds its content after the
dispatcher has staged it.

## Staging and what survives an install

`stage_variant` materializes one selected variant into a per-user, SID-bound
content store and writes three files: the variant's native runtime, its package,
and the artifact index. It creates files and returns the digests it proved. It
opens nothing privileged, reads no registry, and touches no lifecycle state.

The store is per-user and SID-bound, never a machine-wide location, because the
dispatcher that writes it has no elevation and must not need any. Its
directories are verified to be real directories and not reparse points before
anything is written through them, and each records the digest of its own
identity so a substituted directory is detected rather than trusted.

After an install the machine keeps the **selected variant's** maintenance state
and nothing else. The full universal artifact is not retained: a machine that
installed the x64 variant has no copy of the ARM64 one, and `stage_variant`
streams the selected variant's blobs out of the shared store so that is true by
construction rather than by cleanup.

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
`scripts/build-dispatcher.ps1`, `cargo nextest run --workspace --all-features`,
`cargo test --workspace --all-features --doc`, `cargo machete`,
`git diff --check`, `cargo xtask verify-portable-boundaries`,
`cargo xtask github-action-pins check`, a
`zup schema --output schema/zup.schema.json` step that fails when the
checked-in schema differs from the generated one, and
`scripts/verify-frontend-features.ps1`, which proves the three frontend
templates build with the intended dependency graphs and PE subsystems.

The dispatcher is a required input to the composition tests, not something
`cargo test` builds: it is a separate package with a deliberately small
dependency closure, which is the wrong trade for a 128 MB installer and the
right one for a launcher whose size is a design constraint. A test that needs a
dispatcher says so rather than passing without composing anything.

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

`action.yml` covers the GitHub Action itself, and is where the pin-lock gate
lives. It uses two runtimes on purpose: **Bun** installs, tests and bundles the
action, and **Node 24** runs the result, because Node 24 is what the runner
provides.

- `check` — `bun install --frozen-lockfile`, type checking under two tsconfigs
  (the shipped source with no Bun types in scope, the tests with them), `biome
  check`, `bun test`, the bundle build, `git diff --exit-code -- action/dist`, a
  metadata check that `action.yml` and the implementation declare the same inputs
  and outputs, and `node scripts/verify-runtime.mjs`, which *executes* the built
  bundle as a child process.
- `e2e` — the action invoked as `uses: ./` on `ubuntu-latest`, `windows-latest`,
  `macos-15`, `ubuntu-24.04-arm` and `windows-11-arm`, against a locally built
  `zup` supplied through `zup-path` so testing the action does not require having
  released it. It covers setup, build, outputs, a project path containing a
  space, the job summary, and a failing zup failing the step.
- `tool-bootstrap` — opt-in, because it needs a published zup release and reaches
  the network.

Two of those steps exist because the toolchain and the runtime are different
programs. `git diff --exit-code` on `action/dist` catches a stale bundle, and
`verify-runtime.mjs` catches the case Bun and Node disagree — a bundle Bun is
happy with and Node cannot load is a green CI run and a broken release in
somebody else's workflow.

## Current status

The Windows backend is implemented: `zup build` produces a self-contained
installer for every Windows target, or one universal offline installer carrying
several of them, and the runtime installs, updates, repairs, and uninstalls
through the transaction engine and the authenticated worker.

Composition is complete for the offline mode. A thin artifact - one that
carries the index and a launcher and fetches content - is not yet produced;
`[build.artifacts]` accepts `mode = "thin"` and a `channel`, and composition
records both, but nothing fills a thin store yet. `zup-update` still resolves
updates from the release description's variant list rather than from a
content-addressed fetch.

The portable stack is not a claim about Linux support. It is the property that
the semantic model, the build inventory, the artifact graph, planning, the
transaction engine, the package format, plugins, the runtime session, and the
protocol build and test natively on Linux today, in CI, so the Windows adapter
stays an adapter.
