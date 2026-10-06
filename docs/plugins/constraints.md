# Plugin constraints

Plugins are planning extensions, not install scripts.

They do not get a filesystem, network, process or host-command API. They receive
planner context and return typed resources. That keeps their output inspectable
with `zup plan` before the machine is changed.

## Failures

A plugin may return an `Error`, which carries a code a host branches on and a
message a person reads:

```rust
Err(Error::invalid("no shell component is selected"))
```

A refused plugin rejects planning; Zup does not keep a partial set of resources.
Returning an `Error` is the right way to say "not for this context" - it is an
answer the host can show a person, where a trap is a fault.

Runtime failures such as a trap, timeout, memory exhaustion or invalid output
are reported as plugin failures rather than being converted into an incomplete
plan.

## Resource limits

| Limit | Value |
| --- | ---: |
| Resources returned by one plan | 4,096 |
| Encoded plan | 8 MiB |
| Fuel per call | 100,000,000 |
| Wall clock per call | 250 ms |
| Memory | 32 MiB across at most 4 memories |
| Host calls | 0 |
| Plugin error text | 512 bytes |

`Plan::validate()` checks the first of these inside the plugin, so an oversized
plan is caught at the point of the mistake.

Treat these as guardrails, not targets. A plugin generating megabytes of
configuration is usually the wrong abstraction.

For the exact WIT types, use the [Plugin API reference](/reference/plugin-api).