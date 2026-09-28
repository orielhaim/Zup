# Security model

What zup trusts, what it refuses, and where the boundaries are. Read this before
changing anything that parses untrusted bytes.

## The shape of the problem

A zup installer is a file a person downloads and runs, with the privileges of
whoever ran it - usually not an administrator. The interesting attacks are:

1. **Composition.** Bytes from one place end up inside a file that runs with
   another place's trust. A runtime template composed from a developer's
   `target/` directory is a supply-chain attack that looks like a build.
2. **Path escape.** A name from a manifest, catalog, or HTTP response resolves
   against a directory the machine trusts. `..\` in a catalog entry is a write
   outside the content cache.
3. **Protocol confusion.** A parent and a worker talk over a pipe. A frame that
   says "authentication failed" and one that says "installation is busy" must be
   told apart: one is a refusal, the other a retry.
4. **Ambiguity.** Two questions with the same answer where one needed the other.
   A `signed: bool` cannot distinguish "signed" from "unsigned and we know it",
   so signing evidence is a list of independently-failing facts, and "finalized"
   is asked separately from "signed".

Each is closed by a *type* or a *check*, not a convention.

## Untrusted input

| Input | Who writes it | Checked by |
| --- | --- | --- |
| A release directory | whoever unpacked the download | `ToolchainRelease::verify` |
| A toolchain component | `cargo xtask toolchain build` | descriptor + the file's own PE header |
| A content path | a manifest, catalog, or HTTP response | `RelativeContentPath::parse` |
| A release description | `zup build` | `ReleaseManifest::parse` + `is_finalized` |
| An artifact index | a build | `ArtifactIndex::parse` |
| A transport package | a build | `Package::parse` (verifies digests) |
| A wire frame | the other process | `decode_payload` + the sequence tracker |
| A command line | whoever launched the process | `split_command_line` |
| An installer manifest | a person | `zup-manifest::parse` + `compile` |

The rule in every row: **the check happens where the value is turned into an
action**, not where it arrived. A parsed-but-unverified document is not a safe
thing to hold, so none of these types hands one back.

## Path containment

`RelativeContentPath` is deliberately a `/`-separated string rather than a
`Path`, so one rule governs a URL path and a filesystem path. It refuses:
empty, absolute, a drive letter, a backslash, a NUL, an empty segment, and `.`
or `..` as a segment.

The property is not that list but the implication: *if `parse` accepts a path,
no segment of it can leave the root it is joined onto.* A character blacklist is
a claim about the characters somebody thought of; the property test checks the
implication over generated input and a corpus of near-misses.

## Composition

Two things must hold for a composed artifact to be the one its author meant.

**Every input is identified, not just found.** A file name is not a compatibility
check - `zup-setup-gui.exe` is written by every zup release that ever had a GUI
template. Every component ships a machine-readable descriptor beside it, and the
build reads both the descriptor *and* the file's own PE header. A descriptor is a
claim; a PE header is an independent statement; the two agreeing is what makes
the claim worth anything on a host that cannot run the file.

**Resolution is a closed list.** The resolver searches exactly three places - an
explicit root, the cache for this exact zup version, and a toolchain staged
beside the executable - and derives all of them from two inputs. There is no
ambient discovery: not `%PATH%`, not the current directory. A resolver outside a
source checkout cannot find a component inside one. This is asserted as an
*exact list*, because a filter is a claim nothing checks. It is re-checked in
the clean room against the real binary.

## The toolchain cache

`zup toolchain install` reads the index and refuses a directory without one;
refuses a release from another zup version; verifies every named file's size and
digest and cross-checks every component descriptor; copies through a flushed temp
sibling; and then **resolves every component out of the cache the way a build
will**, refusing unless all seven are present and valid.

That last step earns its keep: a cache is a place a bad file can hide from the
person who ran the command, and verifying the copy is the only check that sees
it.

`clean` removes every cached version *except* this executable's own. A machine
can have two zup releases on it, and one deleting the other's components breaks
it, so `--all` is an escape hatch rather than the default.

## Explicit refusals

An unknown value is a protocol error, never a default.

- `Failed.kind` outside `zup_protocol::failure` is a protocol error. The parent
  branches on it to decide between a retry and a refusal.
- A toolchain descriptor with an unknown `format_version` is refused.
- A release description with no `finalized` identity is refused by name.
- A wire frame for another protocol version is refused in both directions.
- A wire frame for an unknown schema is refused rather than half-read.

## The wire protocol

Two processes speak it over a pipe, and the pipe is the only thing between a
session and a worker it did not start.

- **`version` is checked on every frame**, symmetrically.
- **Sequences are strictly increasing.** A repeat or a regression is refused, so
  a replayed frame cannot make a parent apply one plan twice.
- **The worker proves who started it.** `WorkerBootstrap` carries the parent PID
  and a plan hash the worker re-validates independently.

## Locking

The installation lock is keyed by `(application, scope)`, with
`LockScope::{Lifecycle, Bootstrap}` so a prerequisite bootstrap cannot be
mistaken for a lifecycle operation on an application that does not exist yet.

It is deliberately **not** keyed by target or version: those are properties of
one operation, and a key that changed as a plan changed would let two operations
hold "the" lock for the same installation at once.

A crash releases the lock through the OS's handle lifetime, so there is no
stale-PID cleanup to get wrong. This is coordination, not a security boundary.

## Durability

| Property | Claimed | By |
| --- | --- | --- |
| A reader sees the whole file or the previous one | yes | flushed temp sibling renamed over the target |
| A killed process leaves a consistent tree | yes | by construction; a stray hidden temp file is never read |
| The bytes are on the medium before anything points at them | yes | temp file flushed before the rename |
| The directory entry is durable | **no** | Windows cannot open a directory for `FlushFileBuffers` |

Each transaction record is a full snapshot, so one atomic replace is the whole
write; there is no multi-file change to roll forward. Writers serialize on
`transaction.lock` and the revision check runs under that lock.

The same discipline applies elsewhere: staged payloads are `sync_all`ed, the
pre-replace backup goes through `copy_new_durable`, and the GitHub receipt is
written to a temp sibling and renamed.

## Supply chain

`deny.toml` is the written policy: advisories blocking, no ignores, yanked
versions denied, licences allow-listed with `unused-allowed-license = "deny"`,
sources crates.io-only with git refused.

`multiple-versions = "warn"` is deliberate: the graph legitimately contains
upstream-forced duplicates, and a gate refusing all of them would only test its
own exception list.

The gate that holds the line is `cargo xtask verify-dependency-graph`, which
refuses two things `deny` cannot see:

- a workspace package reaching two versions of one external crate - never
  upstream's doing, and from then on a fix lands in one copy and not the other;
- development tooling reaching the graph of a binary that ships. The example
  that matters is `zup-publish-github`: it is a *developer* tool, and the day it
  reaches `zup-installer` through a shared dependency, every user of an Acme
  installer ships a GitHub API client they did not ask for.

Findings print the **path**, not a count. "it is in the installer graph" is a
symptom; "it gets there through zup-distribute-github" is the edge to delete.

## Properties live in the crate that owns the format

Every document format has a writer and a reader in different code on opposite
sides of a trust boundary. When they disagree there is no compiler involved, and
the disagreement shows up as an install that fails on a user's machine.

So each format has a property asserting an *implication*: a document that parsed
re-encodes to itself, an accepted name cannot leave a root, a reported identity is
the identity the value holds. Never a restatement of the parser.

These are ordinary `proptest` cases in ordinary crates, run by `cargo nextest` on
every commit. A property inside a fuzz target needs a nightly toolchain, a
separate workspace, a scheduled job, and a corpus somebody has to remember to
regenerate - five things that can each be quietly skipped. In a test binary, a
property that stopped being generated is a failing build.

A property must also be a real implication. "A compressed blob is never larger
than its logical one" was removed as a property: a Zstandard frame *is* larger
than a tiny payload, so it failed on correct input and taught the next reader
something false. A property that is really a compression-ratio assumption is
worse than none.

## Gates that only test their own exception list

The pattern to watch for: a gate whose failure modes are all in its own skip
list. The fix is to ask the question the tool cannot answer and assert the
*shape* of the answer rather than its absence.

- Exact candidate list, not "nothing under the checkout".
- A unit test per parsing hazard, because each hazard produced a gate that
  silently found nothing (`cargo metadata` node ids come in three shapes; its
  `deps[]` entries carry no version and name the lib target, not the package).
- A test that runs the gate against a violating fixture - a clean report is a
  clean tree only if the parser reads something.

## Roles are packages, not features

`crates/zup` is the developer tool; `crates/zup-installer` is what ships to a
user's machine. A role is a package, because a feature is a compile-time switch
inside one binary and nothing in a test can see which one you got.

`scripts/verify-frontend-features.ps1` proves each presentation builds with the
intended dependency graph and PE subsystem, that no presentation can reach the
build plane, and that `zup` is the only installable binary.

## The clean room

`cargo xtask release clean-room` verifies the release index, creates an empty
project directory outside the workspace, and runs `zup init`, `check`, `doctor`
and `build` there with a **scrubbed** environment - `ZUP_TOOLCHAIN` and
`CARGO_MANIFEST_DIR` explicitly absent, every `CARGO_*` and `RUST*` removed.

The environment is scrubbed rather than extended: an inherited `ZUP_TOOLCHAIN` is
exactly how a broken release looks like a working one, and a `cargo run` that
works while a downloaded binary does not are two different problems.

## What is not covered

Stated plainly, because a security document that only lists strengths is a
marketing document.

- **No revocation beyond `--online-revocation`**, which is off by default. A
  certificate revoked after the timestamp is still accepted offline.
- **No SBOM is published.** The graph is gated and `deny` runs in CI, but a
  consumer is not handed a list of what is inside the installer.
- **The TUF metadata is produced by `tuftool`, not zup.** zup stages the tree
  and signs nothing.
- **Nothing fetches a toolchain over the network.** Resolution is offline by
  design; a build that silently depends on a remote host is a build a release
  engineer discovers is broken during an outage.
- **Generated input is not adversarial input.** A property explores the structure
  it was told about. The parsers are bounded on every length they read, but
  nothing here is a coverage-guided search.
- **No ARM64 CI runner.** The aarch64 path is implemented and compiles, and
  nothing in CI exercises it end to end.
