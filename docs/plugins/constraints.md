# Plugin constraints

Plugins are planning extensions, not install scripts. They receive
planner context and return typed resources — no filesystem, network, process, or host-command API. That keeps their output inspectable
with `zup plan` before the machine is changed.

## Failures

A plugin may return an `Error`, which carries a code a host branches on and a
message a person reads:

```rust
Err(Error::invalid("no shell component is selected"))
```

A refused plugin rejects planning; Zup does not keep a partial set of resources. Traps, timeouts, memory exhaustion and invalid output are reported as plugin failures rather than converted into an incomplete plan.

## Resource limits

| Limit                          |                        Value |
| ------------------------------ | ---------------------------: |
| Resources returned by one plan |                        4,096 |
| Encoded plan                   |                        8 MiB |
| Fuel per call                  |                  100,000,000 |
| Wall clock per call            |                       250 ms |
| Memory                         | 32 MiB per memory, at most 4 |
| WASI host resources            |                           32 |
| Plugin error text              |                    512 bytes |

`Plan::validate()` checks the first of these inside the plugin.

## Native WASI Preview 3

Plugins compile with Rust 1.100 or later to `wasm32-wasip3`. Rust emits a
component directly; no core-module adapter or componentization step is needed.
The desktop host uses Wasmtime 50.0.1 with native async component support.
The planner remains a synchronous WIT function because planning has no external I/O.
Each invocation gets a fresh store, WASI CLI context and resource table, released
on success, error, cancellation or timeout. The host inherits no environment,
arguments, standard streams, terminals, directories or network access. Standard
input is closed and output is discarded. Clock queries and waits trap rather
than exposing nondeterministic state. Filesystem, sockets, random and other
imports are rejected before instantiation.

Both CPU execution and suspended async execution are bounded by cancellation
and the invocation deadline. AOT artifacts include the engine feature configuration
and sandbox policy in their fingerprint; rebuild installers after this migration.

Wasmtime labels its Preview 3 implementation experimental and incomplete, with
no patch-release guarantee for Preview 3-specific fixes. Cooperative guest threads
are not enabled. Rust 1.100 does not yet support them by default. Wasmtime
store limits cover linear memories, tables, instances and the WASI host resource
table, not every runtime allocation, including internal async task and stream
state. This in-process sandbox does not impose a total process memory limit.
The host only
enables the async component features required by native Rust components.

Sources: [Rust 1.100 target support](https://doc.rust-lang.org/1.100.0/rustc/platform-support/wasm32-wasip3.html),
[Wasmtime Preview 3 status](https://docs.rs/wasmtime-wasi/50.0.1/wasmtime_wasi/p3/index.html),
and [Rust 1.100 release notes](https://github.com/rust-lang/rust/releases/tag/1.100.0).

The target is the Rust 1.100 improvement adopted here. The allocator, never-type,
and convenience APIs do not simplify this integration enough to justify unrelated
changes. Cargo build output is located through its JSON artifact messages rather
than its unstable intermediate directory layout.
