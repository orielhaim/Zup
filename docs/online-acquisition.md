# Online acquisition

The network is an untrusted transport. What makes content trustworthy is that an
authenticated release said `sha256:X, size:N` and the bytes hashed to `X` on this
machine. Nothing else is load-bearing.

| Crate | Responsibility |
| --- | --- |
| `zup-acquire` | The engine. Closure computation, verified cache, scheduler, barrier, release graph, web layout. No HTTP, no TUF, no platform APIs. |
| `zup-acquire-http` | The transport. One pooled client, timeouts, redirects, ranges, retry, ordered origins. |
| `zup-update` | TUF. Resolves the release descriptor and hands the engine a closure. |
| `zup-artifact` | Composition and `zup publish stage`. |

`zup-acquire` depends on `zup-core` and nothing else, so the whole engine tests
against an in-memory source and a directory.

## The trust chain

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

Three things authenticate, and they are not the same: **TUF** authenticates the
release graph (a catalog of 4,000 blobs is 551 KiB, 0.215% of the 250 MiB it
describes), **SHA-256** authenticates each blob, and **Authenticode**
authenticates the bootstrap artifact itself. There is no fourth scheme, and no
individual application blob is ever a TUF target - that would mean signing tens
of thousands of entries to convey what the catalog already carries.

`release_digest` is a fingerprint over the canonical body of every other field,
so a release cannot assert an identity that does not describe it.

## The web layout

```text
metadata/                        TUF metadata
releases/<channel>.json          the release descriptor
releases/<channel>/catalog.json  digest-to-size catalog
releases/<channel>/variants/*.json
blobs/sha256/<ab>/<abcdef…>      one compressed blob
tuf-input/…                      the same documents, for tuftool
```

Every object under `blobs/` is named by its identity, so an origin may cache it
forever and a mirror may hold it without being trusted. **A generic CDN, S3, R2,
or a directory served by anything is sufficient.** The server understands no
components, targets, or installation logic.

The same layout is the offline seed format: `--source <directory>` against a
staged tree satisfies a closure with no second packaging format.

## Thin artifacts

One `zup publish stage --thin` emits two files that differ in exactly one byte
of intent: which document they authenticate.

```text
Acme 1.4.0 Web Setup   reads releases/<channel>/versions/1.4.0.json  (immutable, cannot become 1.5.0)
Acme Stable Installer  reads releases/<channel>.json                 (whatever is current)
```

Both are TUF targets and carry **identical bytes**, so the release digest is the
same either way and a pinned and a channel client landing on one version can
prove they would install identical bytes.

A thin artifact carries an index, a detached blob table, the variant manifests,
and a trust block - and **nothing else**. It carries no payload and no native
runtime, even though its index *names* one: the runtime is the thing it exists to
fetch. `zup artifact inspect` reports that honestly, printing `content digests
named` rather than `valid`. Embedding it would make it a slow offline installer
wearing a different name.

The trusted root is **inlined**, not shipped beside the installer, so a
sub-megabyte launcher is a complete trust anchor rather than a download that
must be trusted before it can be checked. It is validated when the project is
materialized, so an unparseable root is a build-time failure.

### The trust boundary

A thin bootstrapper may resolve trusted metadata, select a variant, acquire and
verify a native runtime, create or resume acquisition state, and launch the
verified runtime. It must not write application files, touch the registry,
create services, install prerequisites, or modify `PATH`. Prerequisites are
architecturally separate, not merely absent.

The downloaded runtime is executable code, so it is `ContentKind::Runtime` with
the strictest bounds, verified against a release-authenticated digest *before* it
runs, and the native runtime independently re-derives the release. The
bootstrapper's state is a hint, not an authority.

### Starting the runtime

`CreateProcessW` and nothing else - no `cmd.exe`, no command string, no `PATH`
lookup. The executable path is the one acquisition verified, passed as an
application name *and* as the first token of the command line, which the API
requires for an unquoted path.

Inheritance is a list, not a flag: `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` names the
handles the child may keep, because `bInheritHandles` with an unrestricted list
would hand a network-facing installer every handle the bootstrapper held,
including the TUF datastore's and the cache's open writers. A windowed handoff
inherits nothing; a console handoff inherits exactly the three standard handles,
which is what keeps `stdout`/`stderr` alive for a headless JSONL consumer.

**No correctness depends on the process tree.** On Windows a child is
independent once `CreateProcess` returns; the installation is journalled and
committed in the child's own context. Waiting is a presentation choice - to
forward an exit code and keep a console attached - never a correctness one.

## The state machine

```text
Planning        compute the closure; ask the cache what it already holds
    ↓
Downloading     bounded concurrency, priority order, per-origin failure budgets
    ↓
Verified        every required blob present, digest-proved
    ↓
Staging         decompressed and written to the work root
    ↓
Complete        ── barrier ──▶ the transaction may now mutate the machine
```

Every failure mode terminates in `Planning` or `Downloading`;
`AcquireError::left_machine_unchanged()` is `true` for all of them, and the
`Failed` event carries the flag so a consumer can assert it rather than infer it.

Overlap happens **inside** the barrier. It is fine to create transaction,
quarantine, and staging state first; it is not fine to publish an application
file, a registry entry, or a service while a required download can still fail.

## The resume algorithm

Every byte from any source passes through `ContentCache::writer`, so the algorithm
is the same everywhere.

1. **Open.** If `<digest>.partial` and `<digest>.resume` exist and the record
   describes this descriptor, continue; otherwise start from zero and delete
   what was there.
2. **Measure.** Decompress the accounted wire prefix and hash it; compare
   against the record's prefix digest and length. A mismatch discards the
   partial - a torn write costs a re-fetch, never a corrupt blob. **No hash
   state is ever read from disk.**
3. **Request** `bytes=<offset>-`.
4. **Require a correct range.** A `206` whose `Content-Range` does not start at
   the offset, or whose total is not the descriptor's wire length, is refused. A
   `200` in reply to a range request means the server will not do ranges: the
   partial is **deleted**, not abandoned, and the next attempt starts from zero.
5. **Bound.** The descriptor's wire length is a hard ceiling; a source that keeps
   sending past it is refused rather than allowed to fill a disk.
6. **Verify.** The complete wire form is re-read, decompressed, and hashed.
7. **Publish** `partial → <digest>` by rename. Until that instant no consumer
   outside the cache can see the bytes.

The record is rewritten every 8 MiB, so an unclean exit costs at most that much
redownload. A dropped writer writes a final record from `Drop`, so a graceful
interruption loses nothing.

## The cache

```text
<root>/blobs/sha256/<ab>/<hex>            a verified blob
<root>/blobs/sha256/<ab>/<hex>.partial    a transfer in progress
<root>/blobs/sha256/<ab>/<hex>.resume     what the partial is and how far it got
<root>/blobs/sha256/<ab>/<hex>.lock       a writer's claim
```

No database: a filesystem walk and a digest are enough, and a measurement that
justified one has not been made. Shared across versions, repairs, and different
applications, because identity is cryptographic.

Three properties are enforced, not assumed:

- **Nothing is trusted because it is local.** A blob is re-validated to the depth
  its kind earns: wire length for payload, full decompression and re-hash for
  runtimes, documents, and catalogs. Re-hashing every payload on every read
  would double the cost of every install for a threat that requires write access
  to a directory the process already owns.
- **Partial bytes are never visible.** Publication is a rename after a verified
  hash.
- **No path leaves the root and no link is followed.** Every prefix is checked
  through an injected `CacheFileSystem` so a Windows host can reject reparse
  points.

Concurrent writers are handled by a per-blob claim, which is an optimization and
never a correctness requirement: two writers of one digest produce identical
bytes, so a lost race costs bandwidth. A claim older than `RESERVATION_STALE`
(30 minutes) is reclaimed rather than waited on forever.

### Retention

| policy | keeps |
| --- | --- |
| `temporary` (default) | nothing after the transaction commits |
| `auto` | what a normal install needs to repair or update, minus payload aged past a seven-day exemption |
| `keep` | the whole closure, for a machine that must repair itself offline |

The default is `temporary` because a thin install should not silently double the
application's disk usage. **Eviction can never affect the installed application
or the ownership ledger**: the ledger names destinations, a destination holds
its own copy of the bytes, and a digest is not reachable from either.

Inside the grace period nothing is collectable at all, even under `temporary`,
because a second operation running now may hold a reference no sweep can see.

## The scheduler

Work is taken in scheduler order - **priority first, then smallest first** - so a
2 KiB document the installer is blocked on is not behind a 4 GiB payload blob.
Four priorities: `Critical`, `Normal`, `Payload`, `Background`.

Concurrency is bounded **per origin** (6) and in total (12), not as one global
number, because what limits a transfer is the connection pool of the thing
serving it. Staging is bounded separately (2) and deliberately does *not* bound
transfers: applying the staging depth to the network would make a slow disk
throttle the download.

A transfer runs with no lock held; the chain's lock is held only long enough to
read the live set and record a failure.

Progress is a sampled read of shared counters on an interval, delivered with a
non-blocking send. A slow consumer loses intermediate samples; it cannot lose the
terminal event, which is delivered after every worker settles.

## Retry and failover

Only failures that can change are retried: a reset connection, a temporary DNS or
connect failure, `408`, `429`, and selected `5xx`. A `404` or `403` is a statement
about the request, and repeating it turns a clear refusal into a slow one.

The wait is `backon`'s exponential curve with jitter, bounded by
`BackoffPolicy::maximum`. A `Retry-After` header outranks the local curve when it
is a number of seconds. An HTTP-date is **not** parsed: an installer has no
business trusting a client's clock.

Origin failover is conservative. A source is set aside only after its failure
budget is spent, never after one error, and a success resets it. The only thing a
fast mirror earns is being tried first next time.

Timeouts are separated because they answer different questions: connect (30 s),
response headers (30 s), inter-chunk idle (30 s), whole blob (1 h). A cancelled
wait is noticed within 50 ms.

## One graph, four closures

Install, update, repair, and modify are the same call with a different closure,
and there is one implementation of it:

| Operation | Closure |
| --- | --- |
| install, update | the whole selection |
| `--enable` / `--disable` | the newly-enabled components' content |
| repair | the digests the ledger says drifted |

A repair's closure is arithmetic: the ledger holds each owned resource's digest,
so "drifted" means an owned file with no bytes on disk, or bytes that hash to
something else. The closure is the intersection of that set with what the release
carries - which doubles as the ownership check, because a digest in neither
cannot be asked for. **A one-file repair costs one file.**

A repair with no authenticated source is refused with an explicit *repair source
unavailable*, not with a weakened ownership or integrity check: the ledger is the
authority on what the machine owns, and nothing about a missing file relaxes it.
Every refusal on this path leaves the machine unchanged, and that is asserted.

A runtime installed from a graph holds a **plan-only** package: its plan and none
of its content, because the content came from - and comes again from - the graph.
That package is where the `[updates]` configuration travels, so a machine that
lost its installer file can still repair itself. Its second, third, and fourth
operations take the same path the bootstrapper took, and the embedded plan is
checked against the graph before anything is touched.

## Measured

Two suites, deliberately incompressible fixtures so compression is not quietly
doing the work. Both assert relationships - a ratio, a zero, a count - rather
than absolute timings, which belong to the machine that produced them.

Engine and transport (`zup-acquire-http --test measurements`):

| Measurement | Value |
| --- | --- |
| Update, 5 changed + 1 added of 41 | **7.00 MiB of a 44.0 MB release (16.7%)** |
| Warm cache, same closure | **0 bytes**, 20 cache hits, 21 ms |
| Catalog for 4,000 blobs | **551 KiB**, 0.215% of the content it describes |
| Estimate accuracy | predicted 4.50 MiB, actual 4.50 MiB |
| Peak temporary disk | exactly the wire form, no second copy |
| Scheduler, 16 objects | sequential 500 ms, parallel 458 ms, **1.09×** |

Product path (`zup-dispatch --features online --test measurements`):

| Measurement | Value |
| --- | --- |
| Thin bootstrapper (i686, release) | **4,071,936 B** |
| Online stack cost | **3,132,440 B** over a 938,496 B launcher |
| Composed thin installer, 249.7 MiB release | **launcher + 16 KiB** |
| Handoff document | **634 B** |
| Retention record | 122 B (`temporary`) to 1,188 B (`auto`, `keep`) |

**The 16.7% is the claim the architecture makes, and it is not a timing
measurement** - it is a byte count, and it is exact. The other ~35 objects cost
nothing because the machine already had those exact bytes.

The 1.09× is an honest weak number: these fixtures hash on the transfer path and
hashing is CPU-bound, so the pool cannot overlap it on a loaded machine. It
bounds the scheduler's *overhead*, not its ceiling. What the test asserts is the
relationship that must hold regardless - the parallel pool never finishes later
than the sequential one - and `transfers_run_concurrently_and_the_pool_is_bounded`
observes a peak above 1 and below the configured bound.

### One process, not two

The two-process alternative - a launcher that starts a resolver which starts the
runtime - needs the *same* TUF, HTTP, and acquisition stack plus the 0.90 MiB
launcher, so it is strictly larger, and it adds a second process, a second
Authenticode surface, and a second security boundary for no saving. The
measurement retires this as a judgement call.

A test holds the whole thin file under 8 MiB. That is a hard number on purpose:
it is the constraint the design is measured against, and a future dependency that
pushes past it should fail a test rather than be noticed in the field.

## What is not done

- **The thin bootstrapper has not gained its online path.** `mode = "thin"` and
  a `channel` are recorded by composition and a thin artifact stages correctly
  through `zup publish stage`, but the dispatcher does not yet resolve a release
  and launch a verified native runtime.
- **The updater does not drive the acquisition engine.** `zup-update` still
  resolves updates from a channel descriptor and launches a downloaded
  `maintenance.exe`, so the update path does not benefit from unchanged content
  costing zero bytes.
- **The catalog is the one document that grows with the application.** It is
  bounded independently today; `tough` 0.24 supports neither a delegated nor a
  chunked catalog.
- **Binary deltas and alternative transfer recipes** are not implemented. The
  exact-blob CAS already makes unchanged content free.
- **An HTTPS redirect is never downgraded** in policy, but the redirect handler
  has no test coverage - the existing test routed a redirect on a local server
  and then pointed the client at an unreachable host, so it only proved that an
  unreachable host errors.
