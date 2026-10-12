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
        Ok(Plan::new().path_entry(Path::new("${install}/bin")))
    }
}

zup_sdk::plugin::export!(Configure);
```

`zup plugin build` compiles that for `wasm32-unknown-unknown` and componentises
it. The author never installs a Wasm toolchain or copies a WIT file.

## What a plugin cannot do

The world imports nothing at all, which the host verifies before loading the component. A plugin cannot read a file, run a process, reach the network, get the time or a random number, or write a registry key. Everything it needs arrives in its [context](./context); everything it wants to exist it returns as a
resource.