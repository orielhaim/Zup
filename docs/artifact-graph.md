# The distribution artifact graph

`DistributionVariant` and `DistributionArtifact` are separate types with separate
constructors and separate persistence. A variant is one machine's worth of work -
content, runtime, requirements, trust - built locally, cached, and runnable alone.
An artifact is composed after every variant it needs exists, which is what lets
variants be built on different machines and composed wherever the outputs meet.

`zup build` used to produce one installer per target, so a project shipping x64
and ARM64 published two files each carrying a full copy of the shared runtime,
assets, and licence text. One type, `TargetProfile`, was doing three jobs - naming
a target in a manifest, describing a build's output, and describing a file a user
downloads. Those have different lifetimes.

## The four rules

The graph is OCI-inspired and the tests enforce four rules.

**Deterministic.** Blobs are packed into a table in ascending digest order and
split into fixed 3 GiB segments, so the same content always produces the same
byte layout. Canonical JSON is the only accepted encoding: a document that is
merely *valid* JSON is refused, because a reader must be able to predict the bytes
it will verify.

**Bounds-checked.** Every index, table, and manifest has a declared size limit,
and a reader allocates from a declared count rather than from a length field
inside the data. A corrupt artifact cannot make a reader allocate.

**Content-addressed.** Every descriptor names a SHA-256 digest and a size, and a
blob is hashed before it is exposed, so a corrupt store cannot be laundered into
content that merely happens to parse.

**Forward-versioned and fail-closed.** An index carries a required-feature
bitmask. A reader that does not understand a required feature refuses the whole
artifact rather than installing a subset of it.

## Selection

`zup-artifact::select` returns a typed answer - a `CandidateVariant`, a
`Compatibility`, a `SelectionScore` - not a boolean. The ordering is native before
emulated, and a tie is a refusal rather than a coin flip.

- **Emulation never contaminates installation semantics.** A variant requiring
  native execution is not offered as an emulated fallback, and neither is one
  needing a machine component the host lacks: an emulation layer cannot provide it.
- **Host facts are measured, not inferred.** `zup-windows::host` reads
  `IsWow64Process2` and `GetMachineTypeAttributes`, resolving
  `IsMachineTypeSupported` dynamically from the API set as a tri-state
  `MachineSupport`; `Unknown` resolves against the documented Windows 11
  boundary. `GetVersionExW` is not used because it reports the manifested version;
  `RtlGetVersion` is. Environment variables are never consulted.
- **Selection reads the index, never a file name.**

## The universal artifact

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
index. Everything is inside Authenticode-hashed image sections, because resources
are written before the image is signed; there is no trailing overlay, so a
signature covers the whole artifact.

A thin artifact uses the same identifiers and writes fewer of them - no runtime,
no content store - for the reason above: the runtime is the thing it exists to
fetch. A thin artifact whose index names a runtime is not a contradiction; the
name is what the graph will hand over, and the graph is what has to authenticate
it.

## The dispatcher

`zup-dispatch` is a launcher and nothing else: inspect the host, validate the
index, select a variant, verify and materialize it, start it, forward its exit
code. No registry, no services, no elevation, no prerequisites. Every privileged
operation happens inside the selected native runtime, in its own architecture.

**It is built for 32-bit x86, and composition enforces it.** Windows runs 32-bit
x86 everywhere and 64-bit only where the OS is 64-bit, so the narrowest variant
in an artifact decides how wide a dispatcher may be. `compose_universal_executable`
refuses a dispatcher wider than the narrowest included variant, so this is a rule
and not a convention. i686 is also the smallest of the three machines, which
matters for a file downloaded before any of it has been needed.

| Dispatcher | Size |
| --- | --- |
| `i686-pc-windows-msvc`, offline | **938,496 bytes** |
| `i686-pc-windows-msvc`, online | **4,071,936 bytes** |

The dispatcher links `zup-windows` to share one implementation of the
security-identity query, which pulls the wasmtime closure in through `zup-bundle`'s
plugin validation. **This trade is recorded as unresolved**: ~580 KB of the
launcher's weight buys one implementation of one question, and splitting it out
would add a boundary to keep portable.

## What a machine keeps

`stage_variant` streams the selected variant's blobs out of the shared store, so a
machine that installs from a universal artifact keeps **only the selected
variant's** maintenance state - not the full artifact - by construction rather
than by cleanup.

## Trust flow

1. The release publisher signs the finished file; resources were written before
   signing, so Authenticode covers them.
2. The dispatcher reads and validates the index - bounds-checked, fail-closed on
   unknown required features.
3. Selection is typed and deterministic, driven by the index.
4. `verify_selected_variant` proves the manifest matches its descriptor and every
   blob the variant needs is present, before anything is staged.
5. Staging hashes every blob it reads, so a corrupt store cannot be laundered
   into a package that opens.
6. The native runtime takes over; every lifecycle operation is inside it.

## Measured

`cargo bench -p zup-artifact --bench composition`, on three targets with 270 MiB of
byte-identical shared content and 82 MiB of architecture-specific content each.
The payload is incompressible, so deduplication is the only variable.

| | |
| --- | --- |
| 3 separate installers | 1062.0 MiB |
| one universal artifact | 792.0 MiB |
| saved by composing | 270.0 MiB (**25.4%**) |
| unique blobs in the store | 12 |
| kept after installing on x64 | 354.0 MiB (68.2% of the store) |

**The saving is proportional to how much of an application is shared.** 25% here
is 270 MiB of shared content out of 1.06 GiB. An application whose payload is
almost entirely per-architecture binaries would save little, and the composition
row in `zup check` and `zup doctor` says so before anyone builds one.

On 64 MiB of incompressible data, zstd levels above 9 buy nothing and cost 7×
and 130× the time. **Zstandard is kept** and the default composition level is 9.

## Declined, with the condition that reverses each

**`fastcdc`** recovers sharing between *similar* files. Measured on a 64 MiB
binary across two builds, a rebuilt binary recovers from 0.0 MiB to 64.0 MiB -
total recovery - and the compression cost at a 64 KiB average chunk is 0.3%.
Declined because the sharing it recovers is cross-build and there is no cross-build
cache: within one artifact every variant comes from one build, so files are either
byte-identical (which exact dedup captures completely) or genuinely different. The
second condition is that chunking makes a file a list of reassembled digests rather
than one digest-verified blob, weakening the content-addressed, fail-closed
property the graph exists to provide. **Adopt at 64 KiB when a content-addressed
cross-build cache exists.**

**`object`** was declined in favour of hand-rolled PE header parsing, and is now
adopted with a narrower claim than it could carry. Composition still is not
`object`'s work: it writes into an image that is about to be signed, so a general
rewriter would have to reproduce section alignment, the resource directory and the
certificate table exactly, and `BeginUpdateResourceW` gets that right for free.
Reading an image's own resources is four `kernel32` calls. So `zup-pe` keeps
exactly that: the resource vocabulary composition writes against, the certificate
table, and the byte regions the Authenticode digest measures. Nothing else.

Everything shaped like "what file is this?" moved to `zup-binary`, which is
`object` with zup's vocabulary on top. It reads PE/COFF, ELF, and Mach-O -
including a universal Mach-O's several machines - and answers four questions:
the format, the machines, whether the file records a window or a terminal, and
the operating system the file says for itself. It reads and never writes.

The reason for the change is a target triple, not a parser. A triple names an
architecture, an operating system, a vendor and an ABI, and a binary states only
its architecture and sometimes its operating system. `refuse_target` compares the
two on the fields the file actually stated and leaves the rest unconstrained,
because the alternative - synthesising `x86_64-pc-windows-msvc` from "a PE that is
x86-64" - asserts an ABI the image never recorded and would refuse a perfectly
good MinGW build. **Adding a platform is now target-policy mapping, not writing a
parser.**

A second thing went with it. Six independent tables of PE machine numbers existed
across `zup-pe`, `zup-windows::host`, `zup-xtask`'s clean room, and three test
fixtures; there are none now. So does the duplicated "does this console binary
satisfy this declared frontend" rule, which was written twice and is now one.

**`target-lexicon`** was removed from `zup-artifact`. The portable model stores
canonical triple components as `String` and reconstructs a `TargetTriple` only at
the boundary, so it does not need a triple library. The host side keeps its typed
`HostArchitecture`.

## Build UX

```toml
[build.artifacts.windows]
kind = "universal"          # universal | single
mode = "offline"            # offline | thin
targets = ["windows-x64", "windows-arm64"]
channel = "stable"           # absent means an exact version
output = "Acme-Windows-Setup.exe"
```

A project that declares no artifacts builds one installer per selected target.
Declaring artifacts changes what `zup build` produces and nothing else.

`--target` and `--artifact` answer different questions and are not synonyms:
`--target` names a native variant to build or debug; `--artifact` names a file a
user downloads. `--universal` is shorthand for "compose every selected target into
one file" and is refused alongside `--artifact`.

`zup artifact inspect` reads a built artifact with the same parser the dispatcher
and the runtime use, and verifies every content digest it **carries**, so a report
never describes content the artifact cannot produce. For a thin artifact it carries
none and the report says so.

A release description is written to `dist/zup-release.json`. Its paths are file
names relative to the outputs' shared parent, and an output that does not share a
directory with the others is refused rather than described with a build-machine
path.

## OCI mapping

`zup-artifact::oci` maps the graph onto `oci-spec` 0.10's `ImageIndex` and
`ImageManifest`, and `export_oci_layout` writes a local `oci-layout` directory.
This is an adapter, not a dependency of the installer path, and no OCI client
library is linked. It exists so a consumer who already speaks OCI does not need
zup to read zup's output.
