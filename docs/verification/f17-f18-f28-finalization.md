# Acquisition deadline and observer verification

Baseline: `f07748c2b0e470eba6718f8d622292dddf89a38f`, PR 17.
Verified on Windows x86_64 with Rust 1.100 MSVC and WSL Linux x86_64.

## Contracts

`SourceChain` owns the logical blob deadline. It takes the shortest configured
`ArtifactSource::acquisition_timeout` before trying any source. Every request
receives that absolute Tokio deadline. HTTP capacity waits, redirects, headers,
body consumption, mirrors, retries and backoff remain inside it. Direct HTTP
source calls create a deadline only when the request has none. Phase timeouts
can shorten an attempt. Expiration is `SourceError::Timeout`, never cancellation.
Local-only chains have no timeout or HTTP concurrency permits. A local source
inside a chain containing HTTP shares the chain's whole-blob cap. Synchronous
filesystem operations settle before returning; deadlines cannot preempt them.

`ProgressSink` retains at most the configured number of events. Publication
replaces the oldest event when full and never waits for observers. Each observer
has an independent cursor and skips overwritten samples. Terminal publication
freezes the history. Each live observer reads the retained terminal once, then
reads end-of-stream even while the sink remains alive. A late subscriber starts
at the latest event, including terminal. Dropped observers do not affect work.
`blocking_recv` serves synchronous observer threads; no listener task is detached.
The result of `AcquisitionSession::run` remains authoritative.

Only successful staging increments completion. Acquired counters include cache
and local content. Aggregate counters cannot measure network rate or predict
staging duration, so throughput is zero and ETA is absent. Cache bytes never
become fabricated transfer throughput. HTTP redirects suppress all caller-added
headers on an origin change, including custom credential headers; configured
bearer grants remain scoped to their exact origin.

## Regressions and validation

- Before the observer fix, an empty run with an unread live capacity-one receiver
  exceeded the test's one-second bound after its workers had settled.
- The paused-time two-HTTP-source test, copied into an exported baseline source
  tree, consumed 16 seconds against a ten-second budget. The changed code consumes
  exactly ten seconds. No checkout reset or branch change was used.
- Observer tests cover unread, independent, late and synchronous readers,
  terminal ordering, failure, cancellation and closure. Existing F07 tests retain
  first-error latching and active staging settlement.
- Real filesystem staging failures cover an occupied path and a read-only file.
  Warm-cache failure returns no barrier and preserves verified content and the
  preexisting file. Existing interrupted-write tests retain resumable partials.
- Windows acquisition, HTTP, runtime, session, presentation, dispatch and installer
  run: 175 passed, three skipped. The later parameterized filesystem tests pass
  separately. Installer coverage includes lifecycle and source-free recovery.
- Linux acquisition, HTTP, runtime and session run: 142 passed, one skipped,
  before the read-only-file parameter was added.
- The subsequent Linux lifecycle, HTTP, coordinator and native-cache run passed
  59 tests, one skipped, including read-only-file denial and recovery. Windows
  bootstrap filesystem tests passed all six cases.
- Workspace formatting, all-target Clippy with warnings denied and workspace
  doctests passed. Cargo reports existing manifest warnings.

Reproduction commands:

```powershell
cargo nextest run -p zup-acquire -p zup-acquire-http -p zup-runtime -p zup-session -p zup-presentation -p zup-dispatch -p zup-installer
cargo nextest run -p zup-acquire --test lifecycle
cargo nextest run -p zup-windows --test bootstrap_fs
cargo nextest run -p zup-acquire -E 'test(acquisition_thousand_blob_measurement)' --run-ignored all --success-output immediate
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --doc
```

The Linux run uses the native Linux Cargo toolchain:

```sh
cargo nextest run -p zup-acquire -p zup-acquire-http -p zup-linux --test lifecycle --test deadlines --test transport --test coordinator --test native_cache
```

## Measurement limits and open acceptance items

The existing 1,000-blob measurement ran on the same Windows host, debug profile,
with an exported baseline and a shared Cargo target directory. Baseline cold/warm
times were 11,901/486 ms; changed code measured 11,861/528 ms. Both runs made
exactly 1,000 staging calls in each case, with zero/1,000 cache hits. This uses
an in-memory source and a recording stager. It measures cache filesystem work,
but does not measure native staging writes, physical I/O or mixed-network
performance. Single runs do not establish a throughput change.

F17's unified staging path and failure barrier are covered. F18's cross-source
deadline and F28's nonblocking retained terminal gaps are fixed and reproduced.
The full combined request is not closed: live GUI interaction, HTTP/2 over TLS,
exhaustive redirect/auth/permit/cancellation races, native disk-full injection,
and comparable mixed-network physical-I/O measurements remain unverified.
Read-only-file denial is not a disk-full test. No claim of full F18 or F28
acceptance follows from these green tests. F19 has not been started.
