# Security model

What zup trusts, what it refuses, and where the boundaries are. This is the
document to read before changing anything that parses untrusted bytes.

## The shape of the problem

A zup installer is a file a person downloads and runs. Everything it does
happens with the privileges of whoever ran it, usually not an administrator. The
interesting attacks are not "an attacker made a bad installer" — they are:

1. **Composition.** Bytes from one place end up inside a file that runs with
   another place's trust. A runtime template composed from a developer's
   `target/` directory is a supply-chain attack that looks like a build.
2. **Path escape.** A name from a manifest, a catalog, a release description or
   an HTTP response resolves against a directory the machine trusts. `..\` in a
   catalog entry is a write outside the content cache.
3. **Protocol confusion.** A parent session and a worker talk over a pipe, and
   the pipe is the only thing between them. A frame that says "authentication
   failed" and a frame that says "installation is busy" have to be told apart,
   because one is a refusal and the other is a retry.
4. **Ambiguity.** Two questions with the same answer where one of them needed
   the other. A `signed: bool` cannot distinguish "signed" from "unsigned and we
   know it", and a caller that guesses installs something nobody signed. Signing
   evidence is a list of independently-failing facts for the same reason, and
   "finalized" is asked separately from "signed" because they have different
   answers for the same file.

Each of these is closed by a *type* or a *check*, not by a convention, and each
of those types is named below with the failure it prevents.

## Untrusted input, and where it is checked

| Input | Who writes it | Checked by | What a failure is |
| --- | --- | --- | --- |
| A release directory | whoever unpacked the download | `ToolchainRelease::verify` | the whole directory is refused |
| A toolchain component | `cargo xtask toolchain build` | descriptor + the file's own PE header | the component is refused |
| A content path | a manifest, a catalog, an HTTP response | `RelativeContentPath::parse` | the name is refused |
| A release description | `zup build` | `ReleaseManifest::parse` + `is_finalized` | the publisher refuses |
| An artifact index | a build | `ArtifactIndex::parse` | the installer refuses to select |
| A transport package | a build | `Package::parse` (verifies digests) | the launcher refuses |
| A wire frame | the other process | `decode_payload` + the sequence tracker | the session refuses |
| A command line | whoever launched the process | `split_command_line` | arguments are re-derived |
| An installer manifest | a person | `zup-manifest::parse` + `compile` | `zup check` refuses |

The rule is the same in every row: **the check happens where the value is turned
into an action**, not where it arrived. A parsed-but-unverified document is not
a safe thing to hold, so none of these types hands one back.

## Path containment

`RelativeContentPath` is the one type whose whole job is to refuse a string that
could leave a directory. It is deliberately a `/`-separated string rather than a
`Path`, so the same rule governs a URL path and a filesystem path.

```text
RelativeContentPath::parse(value)
```

refuses: empty; absolute; a drive letter; a backslash; a NUL; an empty segment;
and `.` or `..` as a segment.

The property it is checked against is not the list. It is the implication:

> If `parse` accepts a path, then no segment of it can leave the root it is
> joined onto.

A character blacklist is a claim about the characters somebody thought of. The
eight refusals above are the rule; the implication is what the property test
checks, over generated input and over a corpus that includes every near-miss.

The same reasoning caught a real bug in the command-line quoter, which is
[documented next to the fix](hardening.md#a-blacklist-of-whitespace).

## Composition

Two things must be true for a composed artifact to be the one its author meant.

**Every input is identified, not just found.** A file name is not a
compatibility check: `zup-setup-gui.exe` is written by every zup release that ever
had a GUI template. So every component ships a machine-readable descriptor beside
it, naming the zup release, the machine, and the presentation — and the build
reads the descriptor *and* the file's own PE header, and refuses a component the
two disagree about.

That second half is the point. A descriptor is a claim; a PE header is an
independent statement, and the two agreeing is what makes the claim worth
anything on a host that cannot run the file.

**Resolution is a closed list.** The toolchain resolver searches exactly three
places — an explicit root, the cache for this exact zup version, and a toolchain
staged beside the executable — and derives all of them from two inputs. There is
no ambient discovery: not `%PATH%`, not the current directory, not a
machine-global directory. A resolver outside a source checkout therefore cannot
find a component inside one.

This is asserted as an *exact list* rather than as a filter, because a filter is a
claim nothing checks: a resolver that grew a `%PATH%` arm would still pass a
filter that only looks for paths under a checkout. And it is checked again in the
clean room, against the real binary, from outside the repository.

## The toolchain cache

`zup toolchain install` copies a release into the cache. It:

1. reads `zup-toolchain.json` and refuses a directory without one;
2. refuses a release from another zup version;
3. verifies every named file's size and digest, and cross-checks every component
   descriptor against the index and the index's version;
4. copies each file through a temp sibling, flushes it, and renames;
5. **resolves every component out of the cache the way a build will** and refuses
   unless all seven are present and valid.

Step 5 is the one that earns its keep. A cache is a place a bad file can hide
from the person who ran the command, and verifying the copy is the only check
that sees it.

`clean` removes every cached version *except* this executable's own. A machine
can have two zup releases on it, and one of them deleting the other's components
breaks it — so `--all` is an explicit escape hatch rather than the default.

## Signing and trust

See [signing.md](signing.md) for the full model. The security-relevant summary:

- zup holds no key and calls no signing service.
- `zup sign verify` re-measures the signed file. The published digest is a
  measurement, not a transcription of what the signer said it did, and
  `FinalizedArtifact`'s fields are private so no other construction exists.
- The publisher identity is read out of the PKCS#7's own `SignerInfo` and matched
  against the certificate inside the signature. It is not read from the machine's
  certificate store, which on a timestamped signature would name the timestamp
  authority.
- Structure and trust are asked of different systems. `zup-pe` reads the PE
  format and answers "does the embedded digest cover these bytes" on any host;
  `zup-windows::signing` asks `WinVerifyTrust` and can only answer on Windows. A
  non-Windows verifier records the first and does not pretend to the second.
- A universal artifact's embedded runtime is read back out of the composed PE and
  required to hash to the digest the *signed* runtime claims. A release whose
  outer file is signed and whose embedded runtime is not fails.
- `SigningEvidence` is a list, and an unsigned release is a recorded state with a
  real published identity and an empty list — not the absence of one.
- `zup publish` refuses an unfinalized release by name, and re-measures every
  artifact against the description before uploading.

## The wire protocol

Two processes speak it over a pipe, and the pipe is the only thing between a
session and a worker it did not start.

- **`version` is checked on every frame.** A newer worker is refused, not read as
  an older one. The check is symmetric: an envelope for another version is
  refused in both directions.
- **The failure vocabulary is closed.** `Failed.kind` is a fixed set in
  `zup_protocol::failure`, and an unrecognized kind is a *protocol error*, never a
  default. The parent branches on it to decide between a retry and a refusal, and
  a kind it does not know read as a kind it does is the wrong advice for a
  failure it does not understand.
- **Sequences are strictly increasing.** A repeat or a regression is refused, so
  a replayed frame cannot make a parent apply one plan twice.
- **The worker proves who started it.** `WorkerBootstrap` carries the parent PID
  and a plan hash the worker re-validates independently.

## Durability

The transaction journal is the file that decides whether an installation is the
old state or the new one. What is claimed, and what is not:

| Property | Claimed | By |
| --- | --- | --- |
| A reader sees the whole record or the previous one | yes | `atomic-write-file`: flushed temp sibling renamed over `transaction.json` |
| A killed process leaves a consistent record | yes | by construction; a stray hidden temp file is never read |
| The record's bytes are on the medium before anything points at them | yes | the temp file is flushed before the rename |
| The directory entry is durable | Unix only | Windows cannot open a directory for `FlushFileBuffers` |

Each record is a full snapshot, so one atomic replace is the whole write; there
is no multi-file change to roll forward. Writers serialize on
`transaction.lock`, and the revision check runs under that lock.

## Supply chain

`deny.toml` is the written policy: advisories blocking, no ignores, yanked
versions denied, licences allow-listed with `unused-allowed-license = "deny"`,
sources crates.io-only with git refused.

`multiple-versions = "warn"` and that is a deliberate, documented argument: the
graph legitimately contains 49 duplicate versions forced by upstream crates zup
does not control, and a gate that refused all 49 would be a gate that only tests
its own exception list.

The gate that holds the line is `cargo xtask verify-dependency-graph`, which
refuses two things `deny` cannot see:

- a workspace package reaching two versions of one external crate — never
  upstream's doing, and from then on a fix lands in one copy and not the other;
- development tooling reaching the graph of a binary that ships to users. The
  example that matters is `zup-publish-github`: it is a *developer* tool, and the
  day it reaches `zup-installer` through a shared dependency, every user of an
  Acme installer ships a GitHub API client they did not ask for.

Findings print the **path**, not a count. "it is in the installer graph" is a
symptom; "it gets there through zup-distribute-github" is the edge somebody has
to delete.

## Formats: properties in the crate that owns them

Every document format has a writer and a reader in different code on opposite
sides of a trust boundary. When they disagree there is no compiler involved, and
the disagreement shows up as an install that fails on a user's machine.

So each format has a property, in the crate that owns the format, asserting an
*implication*: a document that parsed re-encodes to itself, an accepted name
cannot leave a root, an identity that was reported is the identity the value
holds. Never a restatement of the parser.

`cargo nextest` runs all of them over generated input on every commit, on the
same host and the same toolchain as everything else. A property in the crate it
guards is a property that cannot be quietly skipped: a format added without one
is a format whose absence is a failing build, not a note in a review.

See [hardening](hardening.md#properties-live-in-the-crate-that-owns-the-format)
for the two real bugs these found, and for the one property that was itself wrong
and was deleted.

## What is not covered

Stated plainly, because a security document that only lists strengths is a
marketing document.

- **No revocation service integration beyond the cache.** `--online-revocation`
  exists and is off by default; a signature whose certificate was revoked after
  the timestamp is still accepted offline.
- **No SBOM is published.** The dependency graph is gated, and `cargo deny` runs
  in CI, but a consumer of a zup-built installer is not handed a list of what is
  inside it.
- **The TUF metadata is produced by `tuftool`, not by zup.** zup stages the web
  tree and signs nothing.
- **Generated input is not the same as adversarial input.** A property over
  `proptest`'s generator explores structure it was told about. The parsers are
  bounded on every length they read, and a property asserts the bound, but
  nothing here is a coverage-guided search for the input nobody imagined.
- **No ARM64 CI runner.** The aarch64 path is implemented and compiles, and
  nothing in CI exercises it end to end.
