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

| Limit | Value |
| --- | ---: |
| Resources returned by one plan | 4,096 |
| Encoded plan | 8 MiB |
| Fuel per call | 100,000,000 |
| Wall clock per call | 250 ms |
| Memory | 32 MiB across at most 4 memories |
| Host calls | 0 |
| Plugin error text | 512 bytes |

`Plan::validate()` checks the first of these inside the plugin.