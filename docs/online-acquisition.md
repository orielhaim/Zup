# Online acquisition

This is the report on zup's acquisition side: what an online install is, why
each boundary is where it is, and what the measurements are. Everything here is
measured or cited.

## The claim

```text
small trusted bootstrapper
    ↓
resolve an authenticated release
    ↓
select exactly one compatible variant
    ↓
compute the required content closure
    ↓
download only the missing immutable blobs
    ↓
verify and stage
    ↓
the existing zup transaction engine, unchanged
```

The network is an untrusted transport. What makes content trustworthy is that a
TUF-authenticated release descriptor said `sha256:X, size:N` and the bytes
hashed to `X` on this machine. Nothing else is load-bearing.

## The pieces

| Crate | Responsibility |
| --- | --- |
| `zup-acquire` | The engine. Closure computation, the verified cache, the scheduler, the barrier, the release graph model, the web layout. No HTTP, no TUF, no platform APIs. |
| `zup-acquire-http` | The transport. One pooled client, timeouts, redirects, range requests, retry, ordered origins. Contributes exactly one thing: a source that fills the cache. |
| `zup-update` | TUF. Resolves the release descriptor from a signed channel target and hands the engine a closure. |
| `zup-artifact` | Composition and `zup publish stage`, which turn one build into the tree a static origin serves. |

The dependency direction is the point: `zup-acquire` depends on `zup-core` and
nothing else, so the whole engine is testable with an in-memory source and a
directory, and a network in the picture is a detail rather than a condition.

## The online trust chain

```text
embedded trusted TUF root          shipped in the installer, capped at 1 MiB
    ↓  expiration enforced, rollback rejected
timestamp.json / snapshot.json     the freshness chain
    ↓
targets.json
    ↓  read_target, bounded, digest-checked by tough
releases/<channel>.json            the release descriptor
    ↓  release_digest recomputed from its own body
releases/<channel>/catalog.json    every blob's wire and logical size
releases/<channel>/variants/*.json one variant's install plan
    ↓
blobs/sha256/<ab>/<hex>            immutable content
```

Three things authenticate, and they are not the same thing:

1. **TUF** authenticates the *release graph*: which blobs exist, what they are
   called, and how big they are. That metadata is small — a catalog of 4,000
   blobs is **551 KiB**, 0.215% of the 250 MiB it describes — so it stays small
   however large the application gets.
2. **SHA-256** authenticates each *blob*. A blob may come from any origin, in
   any order, because the client proves the bytes locally.
3. **Authenticode** authenticates the *bootstrap artifact itself*, which is a
   platform concern and never appears in the acquisition model.

There is no fourth scheme. There is no second application-level signature
format, and no individual application blob is ever a TUF target — that would
mean signing tens of thousands of entries to convey information the catalog
already carries and the client can check itself.

`release_digest` is a fingerprint over the canonical body of every other field,
so a release cannot assert an identity that does not describe it. One version
number can describe different bytes on two machines; the digest is what is
recorded and what is trusted.

## The web and CDN layout

```text
metadata/                        TUF metadata
releases/<channel>.json          the release descriptor
releases/<channel>/catalog.json  digest-to-size catalog
releases/<channel>/variants/*.json
blobs/sha256/<ab>/<abcdef…>      one compressed blob
tuf-input/…                      the same documents, for tuftool
```

Every object under `blobs/` is named by its identity, so an origin may cache it
forever, a client may resume into it, and a mirror may hold it without being
trusted. The two-level fan-out keeps any single directory small enough for a
plain static host. **A generic CDN, S3, R2, or a directory served by anything at
all is sufficient.** No zup application server exists, and the server understands
no components, targets, or installation logic.

The same layout is the offline seed format. `--source <directory>` pointed at a
staged tree satisfies a closure with no second packaging format, which is what
makes a USB stick, an enterprise share, and a pre-warmed CI image the same thing.

## Thin artifacts

Two artifacts, deliberately not the same one. One `zup publish stage --thin`
emits both, and they differ in exactly one byte of intent: which document they
authenticate.

**`Acme 1.4.0 Web Setup`** is version-pinned. It reads
`releases/<channel>/versions/1.4.0.json`, an immutable version-addressed name that
nothing rewrites, and always installs that release. It cannot silently become 1.5.0
six months later.

**`Acme Stable Installer`** follows a channel. It reads `releases/<channel>.json`
and installs whatever is current when it runs.

The distinction is carried in `ReleasePin`, is visible in `pin.label()` and
therefore in the build output and the window title, and has one real consequence:
a version-pinned bootstrapper can name its runtime before it runs, and a channel
bootstrapper cannot, because the release is not known until it resolves.

Both names are published by one `zup publish stage`, are both TUF targets, and
carry **identical bytes** - so the release digest is the same either way, and a
pinned client and a channel client that land on one version can prove they would
install identical bytes. A version the channel has moved past is still
addressable, because nothing removes the version-addressed document; that is what
a pinned installer depends on.

A thin artifact carries an index, a detached blob table, the variant manifests,
and a trust block - and **nothing else**. It does not carry a payload, and it does
not carry a native runtime, even though its index *names* one: the runtime is the
thing the artifact exists to fetch, and embedding it would make the installer the
application. `zup artifact inspect` reports that difference honestly, printing
`content digests  named` rather than `valid` for a thin artifact, because those
digests are authenticated by the release rather than by that file.

The release, by contrast, carries the runtime as ordinary verified content: it is
staged under `blobs/sha256/...` like any other object and goes through the same
verification, which is the only reason a bootstrapper may execute it. A test
asserts both halves - that the staged web tree has a `blobs/` directory holding
the native runtime, and that a thin installer is smaller than its own launcher
plus 256 KiB.

The trusted root is inlined rather than shipped beside the installer. A root is a
few kilobytes of signed JSON, and inlining it is what lets a sub-megabyte
launcher be a complete trust anchor rather than a download that has to be trusted
before it can be checked. The root is read and validated when the project is
materialized, so a publisher cannot ship a launcher with an unparseable root
without finding out at build time.

### The trust boundary

A thin bootstrapper may:

- resolve trusted release metadata
- select a variant for the host
- acquire and verify a native runtime
- create or resume acquisition state
- launch the verified runtime

It must not write application files, touch the registry, create services,
install prerequisites, or modify PATH. Every one of those is inside the native
runtime, under the transaction engine, in its own architecture. Prerequisites in
particular are architecturally separate rather than merely absent: they live in
their own module and hand the engine a payload closure that does not include them.

The downloaded runtime is executable code, so it is a `ContentKind::Runtime` with
the strictest bounds, it is verified against a digest the release authenticated
*before* it runs, and the native runtime independently re-derives the release and
re-validates what the bootstrapper tells it. The bootstrapper's state is a hint,
not an authority.

### Starting the runtime

`CreateProcessW` and nothing else. No `cmd.exe`, no command string, no `PATH`
lookup, no self-replacement: the executable path is the one acquisition verified,
and it is passed as an application name *and* as the first token of the command
line, which is what the API requires for an unquoted path. The thin architecture
does not know `CreateProcessW` exists - the call lives in `zup-windows`, and the
dispatcher holds a `Launcher` rather than calling the API.

Inheritance is a list, not a flag. `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` names the
handles the child may keep, because `bInheritHandles` with an unrestricted list
would hand a network-facing installer every handle the bootstrapper happened to
hold, including the TUF datastore's and the content cache's open writers. A
windowed handoff inherits nothing - the bootstrapper is itself a windowed process
with no console, so the child creates its own window and the user sees one
installer. A console handoff inherits exactly the three standard handles, which
is what makes `stdout` and `stderr` survive for a headless consumer's JSONL
stream. If this process has no console at all, the child gets no standard handles
and `STARTF_USESTDHANDLES` is left unset, rather than a child that is silently
mute.

**No correctness depends on the process tree.** On Windows a child is independent
once `CreateProcess` returns: it reads nothing from the bootstrapper, and the
installation it performs is journalled and committed by the transaction engine in
the child's own context. Waiting is a *presentation* choice - to forward an exit
code and to keep a console attached - never a correctness one. A bootstrapper that
is killed mid-install leaves an installation that is either committed or
recoverable, which is the guarantee the offline artifact has always had.

## The acquisition state machine

```text
Planning        compute the closure; ask the cache what it already holds
    ↓
Downloading     bounded concurrency, priority order, per-origin failure budgets
    ↓
Verified        every required blob present, digest-proved, staged
    ↓
Staging         decompressed and written to the work root
    ↓
Complete        ── barrier ──▶ the transaction may now mutate the machine
```

Every failure mode terminates in `Planning` or `Downloading`. `AcquireError::
left_machine_unchanged()` is `true` for all of them, and the `Failed` event
carries the flag so a consumer can assert the guarantee rather than infer it.

Cancellation is the same: a raised flag is noticed between queue items, between
body chunks, and between retry waits. A retry that ignored it would leave a
cancelled installer waiting out its backoff, which is the difference between a
prompt cancel and one that appears to hang.

## The resume algorithm

Every byte a network or a local source delivers passes through
`ContentCache::writer`, so the algorithm is the same everywhere:

1. **Open.** If `<digest>.partial` and `<digest>.resume` exist and the record
   describes this descriptor, continue; otherwise start from zero and delete what
   was there.
2. **Measure.** Decompress the accounted wire prefix and hash it. Compare
   against the record's prefix digest and prefix length. A mismatch means the
   partial is discarded — a torn write costs a re-fetch, never a corrupt blob.
   **No hash state is ever read from disk.** The record says *where* to continue;
   the bytes themselves are what prove it.
3. **Request.** Ask for `bytes=<offset>-`.
4. **Require a correct range.** A `206` whose `Content-Range` does not start at
   the offset, or whose total is not the descriptor's wire length, is refused.
   A `200` in reply to a range request means the server will not do ranges: the
   partial is **deleted**, not abandoned, and the next attempt starts from zero.
   That is the documented fallback and it terminates.
5. **Bound.** The descriptor's wire length is a hard ceiling. A source that keeps
   sending past it is refused rather than allowed to fill a disk.
6. **Verify.** The complete wire form is re-read, decompressed, and hashed. The
   logical length and the digest must both match.
7. **Publish.** `partial → <digest>` by rename. Until that instant, no consumer
   outside the cache can see the bytes.

The record is rewritten every 8 MiB, so an unclean exit costs at most that much
redownload, and the record itself stays a few hundred bytes. A writer that is
dropped or killed mid-transfer writes a final record from `Drop`, so a graceful
interruption loses nothing at all.

A CDN without range support works correctly: resume simply falls back to a full
download. The test `a_server_that_ignores_a_range_still_works_by_restarting`
proves it costs a re-fetch and not a wrong file.

## The cache model

A per-user content-addressed store, keyed by immutable digest:

```text
<root>/blobs/sha256/<ab>/<hex>            a verified blob
<root>/blobs/sha256/<ab>/<hex>.partial    a transfer in progress
<root>/blobs/sha256/<ab>/<hex>.resume     what the partial is and how far it got
<root>/blobs/sha256/<ab>/<hex>.lock       a writer's claim
```

It is shared across versions, reinstalls, repairs, updates, and different zup
applications with identical content, because identity is cryptographic. **No
database is involved**: a filesystem walk and a digest are enough, and a
measurement that justified one has not been made.

Three properties are enforced, not assumed:

- **Nothing is trusted because it is local.** A cached blob is re-validated to
  the depth the kind earns: wire length for payload, full decompression and
  re-hash for runtimes, documents, and catalogs. Re-hashing every payload blob
  on every read would double the cost of every install for a threat that
  requires write access to a directory the process already owns.
- **Partial bytes are never visible.** Publication is a rename after a verified
  hash.
- **No path leaves the root and no link is followed.** Every prefix of every
  path is checked before a byte is read or written, through an injected
  `CacheFileSystem` so a Windows host can reject reparse points.

Concurrent writers are handled by a per-blob claim, which is an optimization and
never a correctness requirement: two writers of one digest produce identical
bytes, so a lost race costs bandwidth and nothing else. A claim older than
`RESERVATION_STALE` — configurable, 30 minutes by default — is reclaimed rather
than waited on forever.

### Retention

`temporary` holds nothing after the transaction commits. `auto` keeps what a
normal install needs to repair or update without re-fetching unchanged content,
and prunes payload that has aged out. `keep` retains the whole closure, so an
author who wants offline-repair capability asks for it explicitly. The default is
`temporary`, because a thin install should not silently double the application's
disk usage.

**Eviction can never affect the installed application or the ownership ledger.**
The ledger names destinations; a destination holds its own copy of the bytes; a
digest is not reachable from either.

## The parallel scheduler

Work is taken from a queue in scheduler order — **priority first, then smallest
first** — so a 2 KiB document the installer is blocked on is not sitting behind a
4 GiB payload blob. Four priorities exist: `Critical` for the runtime control is
handed to, `Normal` for metadata, `Payload` for the closure, and `Background`
for work that only makes a later operation cheaper.

Concurrency is bounded **per origin** (6) and in total (12), not as one global
number, because what limits a transfer is the connection pool of the thing
serving it. Staging is bounded separately (2) and deliberately does *not* bound
the transfers: applying the staging depth to the network would cap concurrency at
the disk's depth and make a slow disk throttle the download, which is backwards.

The chain is shared by reference and its lock is held only long enough to read
the live set and to record a failure. A transfer runs with no lock held. (The
first implementation held the chain across the whole transfer, which silently
serialized everything and made the concurrency bound meaningless; the test
`transfers_run_concurrently_and_the_pool_is_bounded` now holds a peak above 1 and
below the configured bound.)

Progress is a sampled read of shared counters, emitted on an interval, delivered
with a non-blocking send. A slow consumer loses intermediate samples; it cannot
lose the terminal event, which is delivered after every worker settles. The
tracked counters are bytes complete, bytes total, throughput, ETA, active
transfers, cache hits, and retry state — and nothing per chunk.

## Retry and failover

Only failures that can change are retried: a reset connection, a temporary DNS
or connect failure, `408`, `429`, and selected `5xx`. A `404` or a `403` is a
statement about the request, and repeating it turns a clear refusal into a slow
one — `a_not_found_is_not_retried` asserts one request, not four.

The wait is `backon`'s exponential curve with jitter, bounded by
`BackoffPolicy::maximum`. A `Retry-After` header outranks the local curve, and is
honoured when it is a number of seconds. An HTTP-date is **not** parsed: an
installer has no business trusting a client's clock, and a wrong date produces
either a stall or a stampede.

Origin failover is conservative. A source is set aside only after its failure
budget is spent, never after one error, and a success resets it. The only thing
a fast mirror earns is being tried first next time; it is never trusted for it.

Timeout budgets are separated because they answer different questions: connect
(30 s), response headers (30 s), inter-chunk idle (30 s), and the whole blob
(1 h). A cancelled wait is noticed within 50 ms.

## The staging pipeline

```text
download blob → verify compressed identity → decompress and verify logical
             → stage the final file on the correct volume
```

Those stages overlap: a verified blob is handed to the `BlobStager` as soon as
it lands, so decompression, logical re-verification, and disk writes run
concurrently with the remaining downloads. The slow pipeline — download
everything, then decompress everything, then hash everything, then stage
everything, then install — is exactly what this replaces.

Overlap happens **inside** the barrier. It is fine to create transaction,
quarantine, and staging state first. It is not fine to publish an application
file, a registry entry, or a service while a required download can still fail.

Measured: the cache holds exactly the wire form it was given, and nothing more,
because publication is a rename. Peak temporary disk is the closure plus one
partial — never two copies of everything.

## Update, repair, and modify: one graph, four closures

Install, update, repair, and modify are the same call with a different closure.
There is no second downloader to drift, and `zup/src/graph.rs` is the only place
that computes one:

```text
TrustContext                     the repository, the channel, the trusted root,
                                 the cache, the pin - and nothing else
  ↓
TUF -> the authenticated release descriptor
  ↓
select the variant this machine runs
  ↓
component selection              all / an enable set / a disable set
  ↓
the closure                      what this operation needs, and nothing else
  ↓
the verified cache               filled once, shared by every operation
  ↓
the existing transaction engine  unchanged
```

**Install and update** take the whole selection.

**Modify** takes the closure of the newly-enabled components, so turning on a
component fetches what that component needs rather than the whole application
again. Only content that is new to the machine moves.

**Repair** takes the closure of the digests the ledger says drifted. The ledger
holds each owned resource's digest, so "drifted" is arithmetic: an owned file
with no bytes on disk, or bytes that hash to something else. The closure is the
intersection of that set with what the release carries - which doubles as the
ownership check, because a digest in neither cannot be asked for. A one-file
repair costs one file.

A repair with no authenticated source is refused with an explicit
"repair source unavailable" rather than a weakened ownership or integrity check.
The ledger is the authority on what the machine owns, and nothing about a missing
file relaxes it. A machine that may have to repair itself offline holds the
`keep` retention policy for exactly this reason.

`zup-acquire` reports failure separately from leaving the machine unchanged
(`GraphError::left_machine_unchanged`). Every refusal in the graph path has that
property, and it is asserted: a refusal must not have installed, replaced, or
removed anything.

### What a thin runtime is, on its own

A runtime installed from a graph holds a **plan-only** package: its plan and none
of its content, because the content came from - and comes again from - the
release graph. That package is the runtime's whole identity, and it is where the
`[updates]` configuration travels: a machine that has lost its installer file can
still repair itself, because the repository, the channel, and the trusted root are
compiled into the binary that installed it.

So a thin runtime's second, third, and fourth operations are graph operations, and
they take the same path the bootstrapper took. The embedded plan is checked
against the graph before anything is touched: if the installation in a given
scope belongs to a different application, or has no release identity at all, the
operation says so instead of fetching a different release's bytes.

### What an update costs, measured

Two independent measurements, both over deliberately incompressible fixtures so
compression is not quietly doing the work.

| Fixture | Full release | Closure moved | Fraction |
| --- | --- | --- | --- |
| 41 objects, 5 changed + 1 added | 44,041,569 B | 7.00 MiB | **16.7%** |
| 42 objects, 5 changed + 1 added | 10,748,519 B | 1,572,954 B | **14.6%** |

In both, the other ~35 objects cost nothing because the machine already had those
exact bytes. A complete installer is the whole release, on every machine, for
every update; the graph is only what changed. The first row is the older
acquire-layer fixture; the second is the product-path fixture and is the one that
runs through the same code the bootstrapper uses.

The offline installer is still published, because enterprise and disconnected
installs want one file. It is a *claim* in the release graph, not a separate
package representation, and the updater never needs it.

## Frontend flow

```text
Acme

Downloading
━━━━━━━━━━━━━━━━━━━━ 63%
116 MiB of 184 MiB · 28 MB/s · 3s

Preparing installation…
```

The same `AcquisitionEstimate` backs the GUI line, the console table, and the
JSON event, so the three frontends cannot disagree about what the install will
cost:

```text
Download        184 MiB
Install         526 MiB
Already cached   72 MiB
```

The estimate is **exact, not approximate**: both it and the session compute it
from the same authenticated catalog, and `a_closure_estimate_is_exact_rather_than_
approximate` asserts that the predicted download equals the expected byte count.

The headless contract is a closed set of aggregate events. A frontend switches
on it exhaustively, so adding an event is a visible change:

```text
release_resolved    variant_selected      acquisition_started
download_progress   cache_hit             retrying
acquisition_complete staging_complete      cancelled      failed
```

A `Failed` event carries one line per source that was tried and why it did not
work. "no source could acquire X" is not actionable; "the CDN reset the
connection twice, then the mirror does not carry it" is.

## Dependency decisions

| Dependency | Version | Why |
| --- | --- | --- |
| `reqwest` | 0.13 | Already the workspace version. HTTP/2 over one pooled connection is the multiplexed behaviour this design wants, and many immutable objects over one connection is the point. |
| `tough` | 0.24 | Stays. It includes the security fixes made after the older traversal and delegation issues, and it brings a compatible HTTP stack. Two major `reqwest` versions in one binary is not a price worth paying for a unification nothing currently needs. |
| `backon` | 1.6 | Supplies the exponential curve **and the jitter**. Jitter is the point: N clients that back off by exactly the same amount retry in lockstep, which is how one origin's bad minute becomes every client's. What counts as retryable, what the ceiling is, and when to move to another origin stay zup's policy, because that is policy rather than arithmetic. |
| `zstd` | 0.14 | The measured compression curve is the argument for keeping it. |
| `aws-lc-rs` / `azure_core` / `object` / `fastcdc` / `governor` | — | **Not added.** See below. |

**HTTP/3 is not a milestone requirement.** `reqwest` still treats its HTTP/3 API
as unstable, so adopting it early would be adopting churn. HTTP/2 plus many
immutable objects already gives the multiplexing this design needs.

**No cloud SDKs.** No `aws-sdk`, no `google-cloud`, no `azure_storage`. A generic
static origin is the target, and the repository's own rule is that a dependency
has to earn its place.

**No `governor` yet.** Rate limiting is a policy that a caller may want and that
nothing currently needs. If a metered-network or user-configurable limit is added,
`governor` is the thing to evaluate; it is not added merely because it exists.

**No `fastcdc`.** Already measured and declined; see `artifact-graph.md`. The
condition that changes the answer is a cross-build cache, which this release
model is not.

## Measured results

Two suites, on one developer machine. The fixtures are deliberately
incompressible so compression is not quietly doing the work, and both assert
relationships - a ratio, a zero, a count - rather than absolute thresholds,
because absolute timings belong to the machine that produced them.

From `cargo test -p zup-acquire-http --test measurements -- --nocapture`, over
the engine and the transport:

| Measurement | Value |
| --- | --- |
| Cold install, 14 of 40 objects selected | 14.0 MiB moved, 26 objects never fetched |
| Update, 5 changed + 1 added of 41 | **7.00 MiB of a 44.0 MB release (16.7%)** |
| Warm cache, same closure | **0 bytes**, 20 cache hits, 21 ms |
| Catalog for 4,000 blobs | **551 KiB**, 0.215% of the 250 MiB it describes |
| Estimate accuracy | predicted 4.50 MiB, actual 4.50 MiB |
| Peak temporary disk | exactly the wire form, no second copy |
| Staged tree vs closure | 8.00 MiB vs 8.00 MiB, byte for byte |
| Scheduler, 16 objects | sequential 500 ms, parallel 458 ms, **1.09×** |
| HTTP over a loopback socket, 12 objects | sequential 320 ms, parallel 295 ms, **1.09×** |

From `cargo test -p zup-dispatch --features online --test measurements
-- --nocapture`, over the product path:

| Measurement | Value |
| --- | --- |
| Thin bootstrapper (i686, release) | **4,071,936 B** |
| The online stack costs | **3,132,440 B** over a 938,496 B launcher |
| Composed thin installer, 249.7 MiB release | **launcher + 16 KiB** |
| Warm cache, 8 objects | **0 bytes** |
| Catalog for 41 objects | **5,885 B**, 0.055% of the content it describes |
| Handoff document | **634 B** |
| Retention record | **122 B** (`temporary`) to **1,188 B** (`auto`, `keep`) |

**The 1.09× is an honest weak number and it is worth explaining rather than
hiding.** These fixtures hash on the transfer path, and hashing is CPU-bound, so
the pool cannot overlap it on a loaded machine. The measurement bounds the
scheduler's *overhead*, not its ceiling: the real ceiling appears when the origin
is the bottleneck rather than the CPU, which is the case a loopback server
cannot reproduce. What the test asserts is the relationship that must hold
regardless — the parallel pool never finishes later than the sequential one, and
`transfers_run_concurrently_and_the_pool_is_bounded` observes a peak above 1 and
below the configured bound.

The claim the architecture makes is the **16.7%**, and that one is not a timing
measurement. It is a byte count, and it is exact.

### Thin bootstrapper size

**Measured.** `cargo xtask toolchain build [--profile <name>]` builds both
flavours of the launcher from one source tree, one target
(`i686-pc-windows-msvc`, because a universal artifact's launcher has to start on
the narrowest machine any variant can serve), and one profile, and stages them
side by side under names that carry the flavour. The difference between the two
images is exactly what the online path costs.

| Image | Size |
| --- | --- |
| Offline launcher (a universal artifact's launcher) | **0.90 MiB** |
| Online launcher (a thin installer's launcher) | **3.88 MiB** |
| The online stack: TUF client, HTTP transport, acquisition engine | **2.98 MiB** |

So a thin installer is **3.88 MiB plus a few kilobytes of trust block**, carrying
a release whose declared content is 249.7 MiB. The offline launcher is under a
quarter of the online image, which is why the two images are one image rather
than two products: the online path is the launcher plus a network stack, not a
second implementation of the launcher.

The test asserts the whole file stays under 8 MiB
(`a_thin_bootstrapper_is_the_launcher_plus_the_online_stack`). That is a hard
number on purpose: it is the constraint the design is measured against, and a
future dependency that pushes past it should fail a test rather than be noticed
in the field.

**Why one process.** The two-process alternative — a small launcher that starts a
resolver which starts the runtime — needs the *same* TUF, HTTP, and acquisition
stack, *plus* the 0.90 MiB launcher, so it is strictly larger. It would also add a
second process, a second Authenticode surface, and a second download/verify/launch
hop, and a second security boundary for no size saving. The measurement retires
this as a judgement call: the numbers now say one process.

### The handoff

The handoff is a few hundred bytes. Measured on a 24-object release with a real
release document and a real cache:

| Step | Cost |
| --- | --- |
| The document | **634 B** |
| Read, parse, and check the launcher's digest | **4.9 ms** |
| Prove this image is the one the release names | **139 ms** for 11 MiB, 79 MiB/s |

The two costs are separate claims. Reading the handoff is fixed by the
architecture, so it is a number a design change can be compared against. Proving
the image is a hash of the whole file, so the honest claim is a *rate*: it is one
sequential read, which is why the handoff passes a digest of the image rather than
the image's contents, and why the test asserts the rate rather than a total.

What the handoff carries is the identity and nothing else: the release's own
fingerprint, the digest of the release document's bytes, the catalog's digest, the
variant's manifest digest, the runtime's digest, the target triple, the frontend,
the bootstrap mode, the scope, and a summary of what the fetch cost. No payload
root, no executable path, no URL, and no plan. Every one of those is a *location*
a bootstrapper chose, and a location is a hint.

`a_bootstrapper_cannot_substitute_anything_the_release_authenticated` attempts
every substitution a bootstrapper could make — a different runtime image, a
different variant, catalog, manifest, target triple, document key, and a handoff
that does not match the digest the launcher passed — and asserts each is refused
before the transaction engine is asked for a plan. Writing that test found a real
gap: the target triple, the frontend, and the application identity were never
compared against the release. They are now.

Two identities are needed for the release, and the reason is worth stating because
it looks like a mistake otherwise. `release_digest` is a fingerprint recomputed
from the document's body, and a release document *names* that fingerprint as a
field, so it cannot also be the hash of its own encoding. A content cache can only
be keyed by a hash. So the handoff carries both: `release` is the graph, and
`document` is the cache key — and pointing `document` at different bytes selects a
document whose recomputed fingerprint is not the release the handoff names, which
is what makes it a location rather than an authority.

### Retention, measured

Three policies, three footprints, measured over a 2 MiB closure plus one object
nothing references (`the_retention_policy_is_a_table_of_promises_and_the_footprint_follows_it`):

| Policy | Pinned | Removed | Footprint | Retention record |
| --- | --- | --- | --- | --- |
| `temporary` | 0 | 17 | **122 B** | 122 B |
| `auto` (default) | 16 | 1 | **2.00 MiB** | 1,188 B |
| `keep` (offline repair) | 16 | 1 | **2.00 MiB** | 1,188 B |

The default and offline-repair policies cost the same disk today; they differ in
whether the closure has a deadline. `auto` exempts the installed closure for seven
days and no longer, so a machine's steady-state footprint is about one release
rather than one release per update. `keep` has no deadline, which is the whole
point: a machine that may have to repair itself with no network cannot have its
cache collected out from under it.

The grace period is a different window from the policy, and the two are not
conflated: inside the grace nothing is collectable at all, even under
`temporary` (`the_grace_protects_a_second_operation_and_is_not_the_policy`), because
a second operation running right now may hold a reference no sweep can see.

## Future work

- **Launch before complete.** The acquisition priorities and content groups are
  already shaped for required-first acquisition, so a future version could
  support MSIX-style progressive install. Deliberately **not** implemented here:
  launching while a lifecycle is partially committed needs a much more
  complicated ownership and update model, and the safe pipeline should be solid
  first.
- **Alternative transfer recipes.** A descriptor may eventually offer several
  authenticated ways to reach the same final digest — a full blob, a delta from
  another digest, chunk reconstruction. The exact-blob CAS already makes
  unchanged content free, and the format is not designed to make a full blob the
  only possible representation forever. Binary deltas are **not** implemented;
  revisit them with real cross-version measurements.
- **Signed mirror lists.** `OnlineTrust::mirrors` is an unauthenticated
  preference list today. The shape is in place for a TUF-delegated mirror list
  when a deployment needs one.
- **BITS and Delivery Optimization.** The transport boundary is clean enough that
  a Windows-only transport could become an optional policy for background updates
  and metered networks. Neither is the cross-platform core transport, and neither
  is implemented.
- **Delegate `max_targets_size`.** The catalog is the one document that grows
  with the application. It is bounded independently today; a very large
  application may need a delegated or chunked catalog, which `tough` 0.24 does
  not support.
