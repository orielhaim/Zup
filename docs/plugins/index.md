# Plugins

A Zup plugin is a declarative extension to what an application installs: it computes resources from planning context that static declarations cannot express, such as a configuration file derived from the target or launchers that depend on component selection. Keep fixed resources in `zup.toml`.

## Authoring

A Rust plugin depends on one crate and answers one question:

```toml
[dependencies]
zup-sdk = { version = "0.1.0", features = ["plugin"] }
```

```rust
use zup_sdk::plugin::prelude::*;

struct Configure;

impl Plugin for Configure {
    fn plan(context: Context) -> Result<Plan, Error> {
        Ok(Plan::new().path_entry(PathEntry::new("cli", "${install}/bin")))
    }
}

zup_sdk::plugin::export!(Configure);
```

`zup plugin build` compiles that directly to a native WASI Preview 3
component with `wasm32-wasip3`. The author never installs a Wasm toolchain or copies a WIT file.

## What a plugin cannot do

The planner uses typed WIT bindings. Rust standard library imports use WASI 0.3 CLI interfaces with an empty environment, closed input, discarded output, and no terminal. Clock interfaces link, but clock access traps to preserve deterministic planning. The host rejects all other imports. A plugin cannot read a file, run a process, reach the network, get the time or a random number, or write a registry key. Everything it needs arrives in its [context](./context); everything it wants to exist it returns as a
resource.
