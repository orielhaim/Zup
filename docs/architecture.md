# Architecture

Every stage is platform-neutral until the Windows adapter, and
`cargo xtask verify-portable-boundaries` checks that boundary.

## Pipeline

Each arrow is a typed hand-off. A stage cannot read the previous stage's inputs
from the filesystem, and no stage below the adapter imports a Windows crate.

```text
zup.toml
  │  zup-manifest: select_targets → compile
  ▼
zup_core::Installer                  normalized, target-bound
  │  zup-build: materialize
  ▼
zup_build::TargetBuildPlan           real files: size, SHA-256, destination
  │  zup-artifact: DistributionVariant::resolve → ArtifactComposer
  ▼
DistributionArtifact                 the file a user downloads
  │  backend: stage_variant
  ▼
Portable package                     schema 1, one target
  │  zup-plan: plan
  ▼
zup_plan::InstallPlan                scope, components, resources, privileges
  │  backend: resolve_target          target lowering
  ▼
zup_platform::TargetPlan             concrete target paths, no templates
  │  zup-exec: plan_execution
  ▼
zup_exec::ExecutionPlan              create / replace / no-op / conflict
  │  backend: plan_target_lifecycle
  ▼
zup_transaction::TransactionPlan     graph, journal, receipts
  │  zup-runtime: run_install_control
  ▼
zup_runtime::InstallOutcome          commit, rollback, reboot, cancel
```

`DistributionVariant` and `DistributionArtifact` are deliberately distinct: a
variant is one machine's worth of work, built locally and runnable alone; an
artifact is composed after every variant it needs exists. That separation is what
lets variants be built on different machines and composed wherever the outputs
meet. See [the artifact graph](artifact-graph.md).

## Authoring

`zup-manifest` owns parsing, `schema` validation, the JSON Schema, and the
target matrix, and produces `zup_core::Installer`. The IR has its own identity
and is the only thing the engine consumes, so the authoring syntax can evolve
without touching the runtime.

Every resource declaration is a `Targeted<T>`: the value plus an optional
`targets` list. An empty list means every selected profile; an unknown profile
name is a validation error. Filtering happens once, during compilation, so the
IR contains only what that target installs.

A **target profile** is a friendly name; a **target triple** is canonical
machine identity. `TargetTriple` normalizes through `target-lexicon`
(`x64-windows-msvc` → `x86_64-pc-windows-msvc`) and rejects unknown
architecture or operating system, so two spellings of one target cannot diverge.
Two profiles resolving to the same triple are a manifest error. A profile name
wins over a triple when both match; results are ordered by profile name, so a
build is reproducible.

`--runtime` and `--output` are positional against the selected targets, one of
each per target in manifest order. A miscounted flag names the profiles rather
than leaving the reader to guess. `zup doctor` reports the same resolution
read-only.

`${location.*}` resolves against a semantic `InstallLocation` the backend maps
for the selected scope: `programs`, `user_data`, `shared_data`, `menu`,
`desktop`. The other variables are `app.id`, `app.name`, `app.version`, and
`install`. A template left unresolved at lowering time is an error, not an empty
path.

## Inventory to transaction

`zup-build` walks the source tree without following links, expands `[[files]]`
patterns, and resolves each destination to a `RelativePath`, a size, and a
SHA-256. A source path reached through a reparse point or symlink is rejected
outright, so a package cannot smuggle a trusted-looking path out of its tree.

The portable engine cannot undo a registry write, a service, or a shell link, so
it does not pretend to. `zup_transaction` journals file operations with full
receipts and delegates platform state to the backend as an opaque payload keyed
by `BackendResourceId`. It validates size, journals it, hands it back on verify,
rollback, and reconcile, and never interprets it - so ownership is decided by the
adapter that created it, which is what makes ownership-aware uninstall possible.

## Target binding

Canonical target identity is checked wherever a target crosses a trust or
persistence boundary.

- **Bootstrap.** A `BoundBootstrapPlan` carries its target; a request whose
  target differs from the transaction or bootstrap plan is refused.
- **Plugins.** A component is compiled for the build host triple and embedded
  with that target, the WIT digest, the engine fingerprint, and its AOT format
  version. The loader requires package target, requested target, and host
  compile target to be the same triple.
- **Protocol.** Frames carry `PROTOCOL_VERSION`; a mismatch fails the
  handshake. The parent/worker pair also verifies process identity and the plan
  hash before executing.

## Backend boundary

Targets are classified against the backends this host implements *before* the
source tree is walked, so an unsupported target never costs a payload inventory
first. The classification reads nothing from disk.

| Target | Build host | Result |
| --- | --- | --- |
| Windows triple | Windows | built |
| Windows triple | non-Windows | refused: `backend unavailable` |
| non-Windows triple | any | refused: `backend not implemented` |

`zup doctor` reports the refusal as its `backend` check and continues with the
remaining checks, so one run shows every problem.

There is no Linux backend. A portable triple is accepted by the manifest, parsed,
canonicalized, and then refused at the boundary. The Linux CI job proves the
portable stack compiles and passes natively; it is not evidence of a Linux
backend.

## Portable crates

`cargo xtask emit-portable-matrix` is the authoritative classification of every
workspace member. `verify-portable-boundaries` fails when a portable crate:

- depends on `windows`, `winapi`, or `zup-windows`;
- declares a `[target.'cfg(windows)']` table;
- imports `std::os::windows` in production source;
- names a Windows API (`windows::Win32`, `winapi::`, …);
- branches on `cfg(windows)` in production source;
- reintroduces a Windows identifier such as `Registry`, `CLSID`, `OpenSCManager`;
- spells a Windows concept inside a string literal.

The last two are vocabulary rules and apply to a `domain` matrix only. A crate
whose domain *is* a platform file format is classified as one in the same matrix
every other command reads: `zup-pe` may say `RCDATA`, because a PE parser that
cannot name a PE resource type is not a PE parser. It still may not depend on a
Windows crate or branch on the build host.

The classification is read from the matrix, so a package cannot acquire the
relaxation by a line written next to the code it silences, and a package no
matrix claims gets the strict rules rather than none. Every member must be
classified exactly once.

`zup` and `zup-installer` are Windows-only because they link the adapter, not
because the model or the build pipeline needs a Windows host.

## Public UI crates

Three crates, in a chain, all published:

```text
zup-ui-protocol  the wire format, versioning, and the domain vocabulary
zup-ui-ipc       the portable process transport that carries it
zup-ui-sdk       what a preset is written against
```

`zup-ui-protocol` depends only on `serde`, `serde_json`, `thiserror` and `uuid`,
and owns its own vocabulary rather than re-exporting `zup_core::ComponentId` or
any other engine type. `zup-installer/src/host` is the conversion boundary:
engine types go in on one side, protocol types come out on the other.

`zup-ui-ipc` moves frames between processes on Windows, macOS and Linux. It
uses Servo's `ipc-channel` for the operating system's own IPC rather than
implementing any of it, and it encodes and decodes the protocol envelope itself,
so the wire format stays Zup's rather than becoming a dependency's serde
representation. A preset is a child the host launches and is not sandboxed, so
this transport is not a privilege boundary and does not borrow the elevated
worker's ACL and process-identity machinery; the protections that apply are the
protocol's own — version, session identity, monotonic sequences, bounded
messages, and the host's validation of every action.

`verify-dependency-graph` holds the crate boundary mechanically: it fails when
any of the three reaches a crate that exists only here. `verify-public-crates`
holds the part a dependency graph cannot see — Cargo unifies features across a
workspace, so a published crate can compile on a dependency feature it never
declared and only fail for the first project outside the repository. It
publishes `zup-ui-protocol`, then builds and tests each crate above it from a
directory that is not this workspace, against the *packaged* archive of the crate
beneath it.

## The installer window

The window is a separate process, not a link-time part of the installer.
`zup-installer/src/host` owns the installation state and validates every
`UiAction` against it; `zup-preset-default` is an ordinary preset that depends on
`zup-ui-sdk` and nothing else.

A consequence worth stating: the preset is disposable. If it crashes, the
transaction keeps running and the host can put a new window in front of the same
state, because a `UiSnapshot` is complete and there is nothing to replay.

## Package and executable

`zup-bundle` writes a portable schema-1 package: a 60-byte header, a
SHA-256-protected JSON index, and Zstandard-compressed content-addressed blobs.
`Package::open` verifies the index and every blob before exposing payload,
plugin, or prerequisite data, so the same bytes are readable on any host without
executing anything.

`zup-windows` embeds that package into a PE - the index as resource 1, each blob
after it - and refuses to embed into an already signed executable. Sign the
finished artifact, because Authenticode covers the embedded package.

`stage_variant` materializes one selected variant into a per-user, SID-bound
content store. It creates files and returns the digests it proved; it opens
nothing privileged and reads no registry. The store is never machine-wide,
because the launcher writing it has no elevation. Its directories are verified
to be real directories and not reparse points, and each records the digest of
its own identity.

## Persisted formats

| Persisted form | Version |
| --- | --- |
| `zup.toml` `schema` | 1 |
| Package index and blobs | 1 |
| Artifact index and variant manifests | 1 |
| Content store table | 1 |
| Release description | 1 |
| Process protocol frame | 1 |
| Plugin API | 1.0.0, AOT format 1, Wasmtime 49.0.0 |
| Install ledger, transaction plan, journal, bootstrap state | 1 |
| Automation protocol, doctor report, artifact inspection report | 1 |

Every loader compares the stored version before deserializing; a mismatch is an
error.

## Roles

The developer CLI and the installer runtime are separate packages, not Cargo
features. A feature is a compile-time switch inside one binary; a role is a
different product with a different dependency graph and consumer. `zup` declares
no features at all. The only features in the repository belong to
`zup-installer`: `gui`, `console`, `headless`.

`scripts/verify-frontend-features.ps1` proves each presentation builds alone,
that no presentation can reach the build plane, and that `zup` is the only
installable binary. Do not inspect lightweight templates with all-feature
workspace commands: Cargo feature unification is global within one invocation.

## Continuous integration

`ci.yml` has four jobs. All are `contents: read` with
`persist-credentials: false`.

| job | runner | what it proves |
| --- | --- | --- |
| `windows` | `windows-latest` | fmt, clippy, toolchain build, the whole suite, doc tests, `machete`, `deny`, both graph gates, schema freshness, the frontend-feature gate |
| `clean room` | `windows-latest` | the release directory works with no checkout, no `target/`, no staged runtime, no `xtask` |
| `release size` | `windows-latest` | the bootstrapper budget, on a release image |
| `ubuntu (portable)` | `ubuntu-latest` | the portable stack compiles and passes natively |

The clean room is a separate job because the main Windows job has a populated
`target/` and a staged toolchain, so a run there can only prove what the
repository already proves.

`cargo deny check` is the written policy; `verify-dependency-graph` is the gate
that holds the line, because `deny` can only see one resolved version per crate.
Findings print the offending path rather than a count. See
[security](security.md#supply-chain).

No scheduled jobs. A check that runs on a cadence and not on a change is a check
whose failure nobody is waiting for.
