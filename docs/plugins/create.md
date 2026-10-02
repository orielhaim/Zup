# Create a plugin

The repository includes a minimal Rust example under `examples/plugins/configure`.

## Add the WIT contract

Vendor the `wit/zup-plugin.wit` file from the Zup version you target into your plugin project. Keep that copy pinned with the project; the WIT package version is the plugin ABI.

Point `wit-bindgen` at the directory and export the `plugin` world:

```rust
wit_bindgen::generate!({
    path: "wit",
    world: "plugin",
});
```

Implement the generated planner trait:

```rust
use exports::zup::plugin::planner::{
    Context, GeneratedFile, Guest, InstallationPlan, PluginError, ResourceItem,
};

struct Configure;

impl Guest for Configure {
    fn plan(context: Context) -> Result<InstallationPlan, PluginError> {
        let contents = format!("app={}\\n", context.app_id).into_bytes();

        Ok(InstallationPlan {
            resources: vec![ResourceItem::GeneratedFile(GeneratedFile {
                destination: "${install}/plugin-config.txt".into(),
                contents,
            })],
        })
    }
}

export!(Configure);
```

## Build a component

Install the Rust guest target and a compatible `wasm-tools`:

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-tools --version 1.256.0 --locked
```

Build and componentize:

```bash
cargo build --release --target wasm32-unknown-unknown
wasm-tools component new \
  target/wasm32-unknown-unknown/release/configure.wasm \
  -o configure.component.wasm
```

Check the resulting component:

```bash
wasm-tools component wit configure.component.wasm
```

It must export the Zup planner world expected by the current plugin API.

Next, bind the component to an application in [Configure a plugin](./configure).
