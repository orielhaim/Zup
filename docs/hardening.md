# Hardening

What the repository does to keep itself from being the weakest link, and the
reasons behind the parts that look unusual.

This is not a checklist. Every entry names the failure it prevents, because a
hardening measure whose reason has faded is a rule nobody will keep.

## The repository is a build, not a source of truth

A contributor's `zup` is built from the working tree. A released `zup` is a
directory. The difference matters more than it looks, so the release is a shape
and the clean room checks it:

```text
<release>/
  zup.exe                      the developer CLI
  toolchain/<version>/…        three runtime templates, four launchers, each with
                               a descriptor beside it
  zup-toolchain.json           the index: every file, with its digest
```

```text
cargo xtask release clean-room
```

verifies the index, creates an **empty** project directory outside the
workspace, and runs `zup init`, `check`, `doctor` and `build` in it with a
**scrubbed environment** — `ZUP_TOOLCHAIN` and `CARGO_MANIFEST_DIR` explicitly
absent, every `CARGO_*` and `RUST*` removed — and then asserts:

- the artifact is a real Portable Executable: DOS stub, `e_lfanew` read *through*
  the pointer, PE signature, and the machine type a dispatcher selects on;
- nothing in the release description names the build machine: no `C:\`, no
  `target\`, no `.cargo`, no `registry/src`;
- every component reports `staged` and a path inside the release it was handed.

The environment is scrubbed rather than extended. An inherited `ZUP_TOOLCHAIN` is
exactly how a broken release looks like a working one, and a `cargo run` that
works while a downloaded binary does not are two different problems this has to
be able to tell apart.

It is a separate CI job because the main Windows job has a populated `target/`
and a staged toolchain in it. A run there proves only what the repository already
proves.

## Roles are packages, not features

`crates/zup` is the developer tool. `crates/zup-installer` is what ships to a
user's machine. They were once one package behind one `--features build` flag,
so `cargo run -- --help` failed unless a feature was chosen, and nothing about
that was visible in a test.

A role is now a package. `scripts/verify-frontend-features.ps1` proves each
runtime frontend builds with the intended dependency graph and PE subsystem,
that no frontend can reach the build plane, and that `zup` is the only
installable binary. A test reads every workspace manifest and asserts that
anything building a binary is `publish = false` — so `cargo install` cannot put
an internal component on a user's `PATH` without someone noticing.

## Nothing routes around a gate

A gate is only a gate if a caller cannot route around it. Two places where that
mattered:

**The GitHub Action.** `action/src/artifacts.ts` reads the `built`/`finalized`
shape of a release description and **refuses a manifest with no `finalized`
identity**. A pipeline that ran `build` and `attest` without the `finalize` step
would otherwise attest and publish pre-sign bytes while every log said it was
signed. Attestation also re-derives each subject's digest from the bytes and
refuses a mismatch.

**The generated workflow.** The earlier generator had `compose` upload its
artifact, `sign` run afterwards, and both `attest` and `publish` collect the
**unsigned** tree. The shape is now: with signing, `compose` uploads
`compose-unsigned`, a `sign` job downloads, signs, runs `zup sign verify`, and
uploads `compose`, and `attest` and `publish` depend on `[compose, sign]` and
collect the signed tree. Without signing, `compose` finalizes in place with
`--allow-unsigned` and uploads once — no redundant multi-gigabyte round trip
through a second job for bytes nobody will re-sign.

Five tests in `crates/zup-publish-github/tests/publisher.rs` assert the shape,
including the unsigned case, so a generator change that reintroduces the bug
fails there.

## A file name is not a compatibility check

`zup-setup-gui.exe` is written by every zup release that ever had a GUI template.
So every component ships a machine-readable descriptor beside it, and a build
reads two independent statements:

1. the **descriptor** — which zup release, which machine, which presentation,
   and the digest of the bytes;
2. the **file's own PE header** — read without executing anything, so a build
   host can check a component it cannot run.

The two agreeing is what makes either worth anything. A template from another
zup release, another machine, or another presentation is refused rather than
embedded, and the failure names which of the two disagreed.

A release index proves the bytes; a descriptor states what those bytes are. A
descriptor that disagrees with the index is either a different component or a
different zup release wearing the same name, and only reading both files finds
it.

## A verified operation, or an error

The pattern throughout: a step either completed and was verified, or returned a
typed error. No step reports success it did not check.

- `xtask toolchain package` reads the index back and hashes every file in it
  before returning. Its first run found a real packaging bug: descriptors were
  indexed as components and then had a descriptor demanded beside them.
- `zup toolchain install` resolves every component out of the cache the way a
  build will, and refuses unless all seven are present and valid.
- `ReleaseManifest::finalize` re-measures the signed file and refuses a digest
  that does not match.
- `zup publish` measures each artifact's digest and size itself and cross-checks
  both against the description.
- `zup doctor` exits nonzero when a check fails, and the report names the row
  that owns each failure.

The last one is a rule about *reporting* too. A report that splits one check into
three shows a green row for a file the build would not use, so a check that is
one decision is one row: the `runtime` row covers the descriptor, the digest and
the PE header together, because a build refuses a component that fails any part
of it.

## Explicit refusals beat implicit defaults

An unknown value is a protocol error, never a default.

- `Failed.kind` outside `zup_protocol::failure` is a protocol error. The parent
  branches on it to decide between a retry and a refusal, so a kind it does not
  know read as a kind it does is the wrong advice.
- A toolchain descriptor with an unknown `format_version` is refused rather than
  half-read.
- A release description with no `finalized` identity is refused by name.
- A wire frame for another protocol version is refused in both directions.

## Locking: identity, and what it is not

The installation lock is keyed by `(application, scope)`, with a
`LockScope::{Lifecycle, Bootstrap}` so a prerequisite bootstrap — which happens
before an application exists — cannot be mistaken for a lifecycle operation on
one.

**It is not keyed by target or version.** Those are properties of one operation,
not of the installation, and a key that changed as a plan changed would let two
operations hold "the" lock for the same installation at once.

Two installs of different applications do not block each other, and neither does
a user-scope and a machine-scope install of the same application: they are
different installations with different ledgers, different install directories and
different uninstall entries.

A crash releases the lock through the OS's handle lifetime, so there is no
stale-PID cleanup to get wrong and no window where a dead process's lock outlives
it.

This is coordination, not a security boundary. It stops two lifecycle operations
from mutating one installation's ledger at the same time, and it says nothing
about an adversary.

### A real asymmetry, fixed

A parent session and the worker it elevates used to take *different* keys: the
parent took a lifecycle key on the quarantine root and the worker took a
bootstrap key on the state root. Both could consider themselves the only
bootstrap of an installation. `InstallationLock::key_for` is now the only place a
key is spelled, and a test asserts the two agree on the same volume.

## Busy is a state, not a message

`InstallOutcome::Busy { operation }`, `WorkerError::Busy` and
`ProcessOutcome::InstallationBusy` (exit code 8) are typed. The automation layer
picks its exit code from the typed `kind`, not from an English sentence, and the
GUI treats "busy" as not-an-error: it returns to the options surface with a
message instead of a red dialog.

Before this, "another installation is running" and "the user pressed cancel"
were the same refusal with different prose, and a CI system reading the message
could not tell a retry from a give-up.

## Durability: what is claimed, and what is not

| Property | Claimed | By |
| --- | --- | --- |
| A reader sees the whole file or the previous one | yes | temp file plus `MoveFileExW` |
| A killed process leaves a consistent tree | yes | by construction |
| The bytes are on the medium before anything points at them | yes | the temp file is flushed before the rename |
| The directory entry is durable | **no** | Windows cannot open a directory for `FlushFileBuffers` |

The journal used to skip the file flush, on the grounds that crash atomicity came
from the rename. That is true of a *process* crash and false of a power cut, and
the journal is exactly the file whose contents decide which of two states a
machine is in. The doc comment on `JournalFs` states each of the four rows
separately rather than implying a blanket guarantee.

The same reasoning moved through the rest of the system: staged payloads are
`sync_all`ed, the pre-replace backup goes through `copy_new_durable` rather than
a plain `fs::copy`, the GitHub receipt is written to a temp sibling and renamed,
and the per-variant release-description merge is one `write_durable`.

## `deny.toml` is the policy; the graph gate is the enforcement

`deny.toml` has 49 upstream-forced duplicate versions, so `multiple-versions =
"warn"` with the argument written down. A gate that refused all 49 would be a
gate that only tests its own exception list.

`cargo xtask verify-dependency-graph` is the gate that holds the line, because
`deny` can only see one resolved version per crate. It refuses a workspace
package reaching two versions of one external crate, and development tooling
reaching the graph of a binary that ships.

Its three parsing hazards each produced a gate that found nothing before they
were fixed, so each has a unit test:

- `cargo metadata`'s `resolve.nodes[].deps[]` has **no `version` field**;
- `name` there is the *lib* target (`atspi_common` for the `atspi-common`
  package), so a build-only crate reached under a renamed lib would be invisible;
- node ids come in three shapes — `registry+…#name@ver`, `path+file:///…#ver`
  where only the version sits after the `#`, and `name ver (path+…)`.

And `cargo metadata` always includes dev-dependencies, so `dep_kinds` has to be
filtered or every workspace package appears inside every other one.

## Properties live in the crate that owns the format

Every document format has an implication-checked property, and the property is
in the crate that owns the format. `cargo nextest` runs all of them over
generated input on every commit, on the same host as the rest of the suite, with
the same toolchain.

The properties are ordinary `proptest` cases in ordinary crates on purpose. A
property written inside a fuzz target is checked only by the fuzzer, and the
fuzzer needs a nightly toolchain, a C++ runtime, a separate workspace, a
scheduled job, and a seed corpus that somebody has to remember to regenerate —
five things that can each be quietly skipped, and a property that never runs
finds nothing. In a test binary, a property that stopped being generated is a
failing build.

The consequence for review is that a parser and its property are read in the same
file tree, and a new format cannot be added without its property, because
`cargo nextest` on the workspace runs everything that is there.

Two real bugs came out of this, and both are the kind a review does not find:

- **`quote_arg` listed the whitespace characters it knew about** (`' '`, `\t`,
  `\n`, `\v`) while `split_command_line` splits on `char::is_whitespace()`. An
  argument containing `\r`, `\f` or U+00A0 formatted **unquoted** and read back
  as two arguments — a path that resolves somewhere else. Fixed by quoting on
  `c.is_whitespace() || c == '"'`, which is also strictly safer:
  `CommandLineToArgvW` accepts quotes anywhere, so quoting more than the
  platform requires costs nothing and quoting less costs an argument.
- **`BlobTable::pack` kept duplicate digests**, producing a table its own `parse`
  refuses. Two plan entries for two identical files is ordinary, so the packer
  now collapses repeats, and refuses two descriptions of one digest that
  disagree rather than picking one.

And one property was itself wrong and was removed: "a compressed blob is never
larger than its logical one". A Zstandard frame is larger than a tiny or
incompressible payload, so it failed on correct input and taught the next reader
something false. A property that is really a compression-ratio assumption is
worse than none.

<a id="a-blacklist-of-whitespace"></a>

## Gates that only test their own exception list

The pattern to watch for: a gate whose failure modes are all in its own skip
list. `cargo deny` with 49 skips is one. The ways a metadata parser can silently
read nothing are three. A dependency graph that grew by accident is invisible to
`deny`.

The fix is the same each time — ask the question the tool cannot answer, and
assert the *shape* of the answer rather than its absence:

- exact candidate list, not "nothing under the checkout";
- unit tests for each parsing hazard, because each one produced a gate that found
  nothing;
- a test that runs the gate against a fixture that violates it, because
  `the_boundary_check_reports_a_fixture_violation_and_exits_nonzero` is the only
  proof that a clean report is a clean tree and not a parser that reads nothing.

## What is deliberately not hardened

- **No SBOM is published.** The graph is gated and `deny` runs in CI, but a
  consumer of a zup-built installer is not handed a list of what is inside it.
- **A 30-second release-size budget is `#[ignore]`d and requested explicitly.**
  Deferred, not weakened: a debug image reports a number about debuginfo rather
  than about the design, so the measurement is only true of a release build, and
  producing one is minutes whose only product is a number. The `release size` CI
  job requests it on every PR.
- **Nothing fetches a toolchain over the network.** Resolution is offline by
  design: a build that silently depends on a remote host being reachable is a
  build a release engineer discovers is broken during an outage.
- **The TUF metadata is produced by `tuftool`.** zup stages the web tree; it does
  not sign a repository.

## See also

- [security.md](security.md) — the trust model.
- [signing.md](signing.md) — the release identity.
- [architecture.md](architecture.md) — the system this hardens.
