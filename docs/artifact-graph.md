# The distribution artifact graph

This is the report on zup's output side: what changed, why each decision was
made, and what the numbers are. Everything here is measured or cited; nothing
is asserted from a design document.

## The problem

`zup build` produced one installer per target. A project that shipped x64 and
ARM64 published two files, each carrying a full copy of the managed runtime, the
asset bundle, and the licence text. A user on either machine downloaded all of
it and used half of it. The usual answer — a bootstrapper that downloads the
right installer — turns an offline install into a network operation, which is
the wrong trade for a desktop installer.

The underlying confusion was that one type, `TargetProfile`, was doing three
jobs: naming a target in a manifest, describing a build's output, and
describing a file a user downloads. Those are different things with different
lifetimes.

## The chain

```text
TargetProfile          a friendly name in a manifest
  │  zup-manifest: select_targets, compile
  ▼
ResolvedTargetConfig   one canonical target, resolved
  │  zup-build: materialize
  ▼
zup_build::TargetBuildPlan    real files: size, SHA-256, destination
  │  zup-artifact: DistributionVariant::resolve
  ▼
DistributionVariant    one machine's worth of work — content, runtime,
                       requirements, trust. Built locally, cached, runnable alone.
  │  zup-artifact: ArtifactComposer
  ▼
ArtifactGraph          content-addressed descriptor graph with a shared store
  │  zup-windows: compose_universal_executable
  ▼
DistributionArtifact   the file a user downloads
```

`DistributionVariant` and `DistributionArtifact` are separate types with
separate constructors, separate persistence, and separate reasons to exist. A
variant is the unit of parallel work and the unit of caching. An artifact is
composed after every variant it needs exists, which is what lets variants be
built on different machines and composed wherever the outputs meet.

## The four rules

The graph is OCI-inspired and holds to four rules that the tests enforce.

**Deterministic.** Blobs are packed into a table in ascending digest order and
split into fixed 3 GiB segments, so the same content always produces the same
table and the same byte layout. Canonical JSON is the only accepted encoding:
a document that is merely *valid* JSON is refused, because a reader must be able
to predict the bytes it will verify.

**Bounds-checked.** Every index, table, manifest, and metadata document has a
declared size limit, and a reader allocates from a declared count rather than
from a length field inside the data. A corrupt or hostile artifact cannot make a
reader allocate.

**Content-addressed.** Every descriptor names a SHA-256 digest and a size. A
blob is hashed before it is exposed, so a corrupt store cannot be laundered
into content that merely happens to parse.

**Forward-versioned and fail-closed.** An index carries a required-feature
bitmask. A reader that does not understand a required feature refuses the whole
artifact rather than installing a subset of it.

## Selection

`zup-artifact::select` returns a typed answer — a `CandidateVariant`, a
`Compatibility`, a `SelectionScore` — not a boolean. The ordering is native
before emulated, and a tie is a refusal rather than a coin flip. The host side
is equally typed: `HostPlatform`, `CandidateVariant`, `Compatibility`,
`SelectionScore`.

Three decisions here are load-bearing:

- **Emulation never contaminates installation semantics.** A variant that
  requires native execution is not offered as an emulated fallback, and neither
  is one that needs a machine component the host lacks, because an emulation
  layer cannot provide it.
- **Host facts are measured, not inferred.** `zup-windows::host` reads
  `IsWow64Process2` and `GetMachineTypeAttributes`, resolving
  `IsMachineTypeSupported` dynamically from the API set and modelling the answer
  as tri-state `MachineSupport`; `Unknown` resolves against the documented
  Windows 11 boundary. `GetVersionExW` is not used because it reports the
  manifested version rather than the real one; `RtlGetVersion` from ntdll is.
  Environment variables are never consulted.
- **Selection reads the index, never a file name.** A file name is a publishing
  convenience. The index is the only thing that decides what runs.

## The universal Windows artifact

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
arithmetic so a reader knows what each identifier holds without consulting the
index. Everything is inside Authenticode-hashed image sections, because
resources are written before the image is signed; there is no trailing overlay,
so a signature covers the whole artifact.

### A thin artifact's resources are a strict subset

A thin artifact uses the same identifiers but writes fewer of them, and the reason
is not an optimisation - it is that the runtime is the thing it exists to fetch:

```text
resource 1              the artifact index
resource 2              the content store table
resources 3..           one variant manifest per variant
                        no native runtime: the index names one, the artifact
                            does not carry it
                        no content store: the table is detached
```

`UniversalLayout` owns the identifier arithmetic either way, so a reader knows
what each identifier holds without consulting the index, and `UniversalArtifact`
reads a runtime resource only when the artifact's mode says it carries one. A
thin artifact whose index names a runtime is not a contradiction: the name is what
the graph will hand over, and the graph is what has to authenticate it.

Embedding the runtime in a thin artifact would make it a slow offline installer
wearing a different name. The offline case is the opposite, and the reason an
offline artifact must be able to execute what it holds.


### The dispatcher

`zup-dispatch` is a launcher and nothing else: inspect the host, validate the
index, select a variant, verify and materialize it, start it, forward its exit
code. It holds no lifecycle authority — no registry, no services, no
elevation, no prerequisites. Every privileged operation happens inside the
selected native runtime, in its own architecture, which is what lets a launcher
this small run under an emulation layer on a machine whose native variant is
something else.

**It is built for 32-bit x86**, and composition enforces it. Windows runs
32-bit x86 everywhere and 64-bit only where the operating system is 64-bit, so
the narrowest variant in an artifact decides how wide a dispatcher may be: a
host that can run the narrowest variant must be able to start the dispatcher
first. The measured consequence:

| Dispatcher | Size |
| --- | --- |
| `i686-pc-windows-msvc`, offline | **940,544 bytes** |
| `i686-pc-windows-msvc`, online (a thin installer's launcher) | **4,060,672 bytes** |
| `x86_64-pc-windows-msvc`, offline | 1,175,040 bytes |

i686 is both the only shape that is correct for every variant set a project
might publish, and 17% smaller. `compose_universal_executable` refuses a
dispatcher wider than the narrowest included variant, so this is a rule and not
a convention.

The dispatcher links `zup-windows` to share one implementation of the
security-identity query, which pulls the wasmtime closure in through
`zup-bundle`'s plugin validation. The `zup-artifact` + `zup-bundle` + `zup-pe` +
`zup-core` closure alone measures 397 KB. **This trade is recorded as
unresolved**: ~580 KB of the launcher's weight buys one implementation of one
question. Splitting the identity query into a fourth crate would cut it, at the
cost of one more boundary to keep portable. It is not done because the
correctness argument for a single implementation is currently stronger than
the 580 KB.

### Staging, and what a machine keeps

`stage_variant` materializes the selected variant into a per-user, SID-bound
content store and writes three files: that variant's native runtime, its
package, and the artifact index. It creates files and returns the digests it
proved. It opens nothing privileged and reads no registry. The store is
per-user and never machine-wide, because the dispatcher writing it has no
elevation and must not need any.

A machine that installs from a universal artifact keeps **only the selected
variant's** maintenance state. It does not retain the full artifact.
`stage_variant` streams the selected variant's blobs out of the shared store, so
that is true by construction rather than by cleanup.

## Measured results

`cargo bench -p zup-artifact --bench composition`, on three targets
(x86, x64, ARM64) with 270 MiB of byte-identical shared content, 82 MiB of
architecture-specific content per target, and a 1 MiB runtime template per
target. The payload is incompressible by construction, so deduplication is the
only variable.

| | |
| --- | --- |
| 3 separate installers | 1062.0 MiB |
| one universal artifact | 792.0 MiB |
| saved by composing | 270.0 MiB (**25.4%**) |
| unique blobs in the store | 12 |
| blobs shared by more than one variant | 3 |
| kept after installing on x64 | 354.0 MiB (**68.2%** of the store) |
| not kept: other machines' content | 166.0 MiB |

Composition is roughly linear in the number of variants — 917 ms, 1.60 s, 2.24 s
for one, two, and three — because the second and third variants add only their
own exclusive content. Metadata encoding is negligible: the index is 3.8 µs, the
blob table 5.0 µs, all three variant manifests 250 ns.

**The saving is proportional to how much of an application is shared.** 25% here
is 270 MiB of shared content out of 1.06 GiB. An application whose payload is
almost entirely per-architecture binaries would save little, and the
composition row in `zup check` and `zup doctor` says so before anyone builds
one.

### Compression

Measured on 64 MiB of incompressible data, which is the honest worst case:

| zstd level | Time | Stored |
| --- | --- | --- |
| 1 | 55 ms | 64.0 MiB |
| 3 | 64 ms | 64.0 MiB |
| 9 | 112 ms | 64.0 MiB |
| 15 | 816 ms | 64.0 MiB |
| 19 | 14,868 ms | 64.0 MiB |

Levels above 9 buy nothing on incompressible data and cost 7× and 130× the
time. **Zstandard is kept** and the default composition level is 9. Replacing it
was considered and rejected: the level curve is the argument for keeping it, not
an argument for changing it.

## fastcdc 5.0.0: measured, and declined

Zup deduplicates by SHA-256 over whole files. That is exact — two files share
nothing unless they are byte-identical. Content-defined chunking finds sharing
between *similar* files, which is the case a rebuilt binary creates.

Measured on a 64 MiB binary, two builds of the same source, with a PE-style
build timestamp in the header:

| Case | Exact dedup | Chunked (8 KiB) | Chunks | Cut time |
| --- | --- | --- | --- | --- |
| identical build | 64.0 MiB | 64.0 MiB | 13,476 | 45 ms |
| rebuilt, newer timestamp | **0.0 MiB** | **64.0 MiB** | 13,476 | 45 ms |
| rebuilt, section reordered | **0.0 MiB** | **64.0 MiB** | 13,477 | 43 ms |

The recovery is total. The cost, on content that compresses, is where the
decision is made:

| Average chunk | Chunks | Stored | Whole file | Overhead |
| --- | --- | --- | --- | --- |
| 8 KiB | 7,853 | 11.0 MiB | 10.4 MiB | +5.7% |
| 64 KiB | 629 | 10.4 MiB | 10.4 MiB | +0.3% |
| 256 KiB | 137 | 10.4 MiB | 10.4 MiB | −0.0% |

**Decision: do not adopt.** The measured benefit is large and real, and it does
not apply to zup today, for two reasons.

1. **The sharing it recovers is cross-build, and zup has no cross-build cache.**
   Within one artifact every variant comes from one build of one source tree, so
   files are either byte-identical — the shared assets, which exact dedup
   captures completely — or genuinely different. Exact dedup already finds 100%
   of the available sharing. `crates/` contains no build cache today, so a
   rebuilt binary is stored once per artifact and chunking buys nothing.
2. **The cost is a second addressing dimension that contradicts the rules
   above.** With chunking, a file is no longer one digest-verified blob; it is a
   list of chunk digests that must be reassembled and re-verified in order. That
   weakens precisely the property — content-addressed, bounds-checked,
   fail-closed — that the artifact graph exists to provide, in exchange for
   sharing across builds that zup does not yet perform.

The condition that changes the answer: when zup gains a content-addressed
cross-build cache, adopt fastcdc 5 at a **64 KiB average chunk size**, where the
measured compression cost is 0.3% and the recovery is complete. Below 64 KiB the
framing overhead is real.

## `object` 0.40: measured, and declined

`object` would replace hand-rolled PE header parsing. It was evaluated against
two jobs and declined for both.

- **Writing resources.** Composition writes into an image that is about to be
  signed. A general object-file rewriter would have to reproduce section
  alignment, the resource directory, and the certificate table exactly — and the
  certificate table is what Authenticode later covers. `BeginUpdateResourceW`
  gets that right for free, which is why `zup-pe` calls it.
- **Reading resources.** Reading an image's own resources is how a program finds
  the artifact it was built into, and that is four `kernel32` calls.

`zup-pe` is 445 lines and is shared by `zup-windows` and `zup-dispatch`, so
there is one implementation of the certificate table. The build-time question
`object` would answer — what format and machine is an arbitrary template — is
already answered by the same header read.

## Build UX

```toml
[build.artifacts.windows]
kind = "universal"          # universal | single
mode = "offline"            # offline | thin
targets = ["windows-x64", "windows-arm64"]
channel = "stable"           # absent means an exact version
output = "Acme-Windows-Setup.exe"
```

A project that declares no artifacts builds one installer per selected target,
which is the behaviour before artifacts existed. Declaring artifacts changes
what `zup build` produces and nothing else.

`--target` and `--artifact` are semantically distinct and the CLI keeps them
that way: `--target` names a native variant to build or debug, `--artifact`
names a file a user downloads. `--universal` is the shorthand for "compose
every selected target into one file" and is refused alongside `--artifact`.

`zup artifact inspect <ARTIFACT> [--format <human|json>]` reads a built artifact
with the same parser the dispatcher and the runtime use, and verifies every
content digest it **carries**, so a report never describes content the artifact
cannot produce. For a thin artifact it carries none, and the report says so:
`content digests  named` rather than `valid`, because those digests are
authenticated by the release rather than by that file. The JSON report is
versioned (`report_version: 1`).

A release description is written to `dist/zup-release.json`. Its paths are file
names relative to the outputs' shared parent, and an output that does not share
a directory with the others is refused rather than described with a build-machine
path.

## Trust flow

1. The release publisher signs the finished file. Authenticode covers the
   resources, because they were written before signing.
2. The dispatcher's first act is to read and validate the index, which is bounds-
   checked and fail-closed on unknown required features.
3. Selection is typed and deterministic, driven by the index.
4. `verify_selected_variant` proves the manifest matches its descriptor and that
   every blob the variant needs is present, before anything is staged.
5. Staging hashes every blob it reads, through the verified store, so a corrupt
   store cannot be laundered into a package that opens.
6. The native runtime takes over. Every lifecycle operation is inside it, in its
   own architecture, under the transaction engine.

## OCI mapping

`zup-artifact::oci` maps the same graph onto `oci-spec` 0.10's `ImageIndex` and
`ImageManifest`, and `export_oci_layout` writes a local `oci-layout` directory.
This is an adapter, not a dependency of the installer path, and no OCI client
library (`oras`, `oci-client`) is linked. The reason it exists is that a
consumer who already speaks OCI should not need zup to read zup's output.

## Dependencies, and why each earned its place

| Dependency | Version | Why |
| --- | --- | --- |
| `oci-spec` | 0.10 | Maps the graph onto a standard shape for export. `default-features = false`; only `distribution` and `image`. |
| `criterion` | 0.8 | The composition benchmark. Dev-only. |
| `fastcdc` | — | **Declined.** Measured above. |
| `object` | — | **Declined.** Measured above. |

`target-lexicon` was removed from `zup-artifact`. The portable artifact model
stores canonical triple components as `String` and reconstructs a `TargetTriple`
only at the boundary, so the model does not need a triple library: 0.13.5 has
tuple variants, no `Ord`, and version churn in a crate that is otherwise
load-bearing for canonical identity. The host side keeps its typed
`HostArchitecture`. `zup-dispatch` shed `semver`, `serde_json`, `sha2`,
`zup-bundle`, and `zup-pe` once the launcher stopped needing them.

## What is not done

- **The online acquisition path is not yet wired into the updater.** The engine,
  the transport, the release graph, the verified cache, the web-tree export, and
  `zup publish stage` are built and tested end to end, and `docs/updates.md`
  documents the workflow they implement. `zup-update` still resolves updates
  from a channel descriptor and launches a downloaded Setup.exe rather than
  driving the acquisition engine, so the update path does not yet benefit from
  unchanged content costing zero bytes. `docs/online-acquisition.md` records the
  measured cost of the graph path and what remains to be connected.
- **The thin bootstrapper has not gained its online path.** `mode = "thin"` and
  a `channel` are recorded by composition, and a thin artifact stages correctly
  through `zup publish stage`, but the dispatcher does not yet resolve a release
  and launch a verified native runtime. The trust boundary it must keep is
  specified in `docs/online-acquisition.md`.
- **A cross-build cache.** The measurement above is what a build cache would
  need to be worth building.
- **The dispatcher size trade.** Recorded above as unresolved, and unchanged by
  this milestone: the online path adds a TUF client and an HTTP stack to a
  launcher that is currently 978 KB, and that cost has not been measured against
  a two-process alternative.
