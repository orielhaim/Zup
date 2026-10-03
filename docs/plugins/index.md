# Plugins

A Zup plugin is a declarative extension to what an application installs.

The installer asks the plugin for additional resources while building the
installation plan. The plugin returns a declaration; Zup owns their install,
repair, upgrade, modify and uninstall lifecycle.

Use a plugin when a resource must be computed from planning context. Keep static
resources in `zup.toml`.

## Good plugin jobs

- generate a configuration file from app version, target or selected components;
- declare launchers or services that depend on component selection;
- derive protocol or file-association resources from plugin-owned logic.

## Poor plugin jobs

- copy a fixed file already known to the project;
- run arbitrary install scripts;
- mutate the target machine directly;
- duplicate resources the manifest can already declare.

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

## The contract

The canonical cross-language ABI is the WIT world `zup:plugin@1.0.0`, and a
plugin implements exactly one operation:

```wit
plan: func(context: context) -> result<installation-plan, plugin-error>;
```

`zup-sdk` is the official Rust binding for it. The world is WIT rather than a
Rust-only interface so another Component Model toolchain can implement the same
contract - see the [Plugin API reference](/reference/plugin-api).

## What a plugin cannot do

The world imports nothing at all, which the host verifies before it will load
the component. A plugin cannot read a file, run a process, reach the network,
get the time or a random number, or write a registry key. Everything it needs
arrives in its [context](./context); everything it wants to exist it returns as a
resource.

Follow [Create a plugin](./create), then [Configure a plugin](./configure).