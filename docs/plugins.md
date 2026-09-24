# Plugins

Zup plugins are planner-only WebAssembly components. The checked-in [`wit/zup-plugin.wit`](../wit/zup-plugin.wit) file is the contract; use or point to that file instead of maintaining a divergent copy.

## Contract

A component implements the `plugin` world's `zup:plugin/planner@1.0.0` interface and exports its `plan` function. It must have zero imports. The planner receives explicit application, install, scope, host, and selected-component facts and returns typed installation resources or a typed `plugin-error`.

The [`configure` example](../examples/plugins/configure) uses `wit-bindgen` 0.62 and points directly at the root WIT:

```rust
wit_bindgen::generate!({
    path: "../../../wit",
    world: "plugin",
});
```

Rust is an example authoring language, not the ABI. Other Component Model toolchains can target the same WIT as their language support matures.

## Manifest

Declare a component in `zup.toml`:

```toml
[[plugins]]
id = "configure"
source = "plugins/configure.component.wasm"
component = "full"
when = "component(\"full\")"
```

`id` identifies the binding and `source` is a project-relative path to the component. The optional `component` and `when` fields shown above scope activation to component selection and a condition.

## Build the example

Install the guest target and a compatible `wasm-tools` if they are not already available:

```text
rustup target add wasm32-unknown-unknown
cargo install wasm-tools --version 1.256.0 --locked
```

From the repository root, build the core module and componentize it:

```text
cargo build --release --target wasm32-unknown-unknown --manifest-path examples/plugins/configure/Cargo.toml
wasm-tools component new examples/plugins/configure/target/wasm32-unknown-unknown/release/configure.wasm -o examples/plugins/configure/target/wasm32-unknown-unknown/release/configure.component.wasm
wasm-tools component wit examples/plugins/configure/target/wasm32-unknown-unknown/release/configure.component.wasm
```

The last command must show a root world with `export zup:plugin/planner@1.0.0;` and no `import` declarations. The intermediate and component outputs are build artifacts under the example's ignored `target` directory; do not check in either binary.

## Build an installer

The default `zup` package is runtime-only. Build the compiler-enabled CLI explicitly when authoring an installer:

```text
cargo build -p zup --features build --bin zup
cargo build -p zup --no-default-features --bin zup-setup
```

Run the first command's `zup` binary with the manifest and installer target. The `<TRIPLE>` is the AOT/runtime target, not the guest's `wasm32-unknown-unknown` target. `zup build` resolves and hashes the source, rejects a core module, validates zero imports and the exact planner export and signature, AOT-compiles the component, verifies the AOT output, and embeds it with its target, WIT, engine, size, and digest metadata. The runtime-only build is safe for production and does not include the plugin compiler.

## Runtime and safety

The installer runtime loads only the verified AOT component embedded in the bundle and uses the Wasmtime runtime with Component Model support only. It does not provide WASI, environment, clock, filesystem, network, randomness, UI, or other host imports. Source-manifest lifecycle mode does not JIT an active plugin; use an embedded package built by `zup build`.

AOT bytes are native-code artifacts and are trusted only to the same extent as the containing Setup package; bundle hashes detect corruption, while release authenticity comes from Authenticode/TUF. Bundle self-hashes do not prove publisher provenance.

The sandbox disables nondeterministic and concurrent WebAssembly features and limits each invocation to 100,000,000 fuel, a 250 ms deadline, a 1 MiB stack, 512 memory pages (32 MiB), four memories, four tables, 10,000 table elements, and eight instances. Plans are limited to 4,096 resources and 8 MiB of output; generated files are limited to 1 MiB each and 8 MiB in aggregate. One bundle may contain at most 128 plugin components and 256 MiB of uncompressed AOT data.

Failures remain typed: `Rejected { code, message }`, `Cancelled`, `Trap`, `FuelExhausted`, `MemoryLimit`, `Timeout`, `OutputLimit`, `InvalidOutput`, and `Internal`; resource-count violations are reported as bounded resource failures. A returned generated file is hashed and merged into the ordinary installation plan, so install, ownership, repair, upgrade, and uninstall use the same transaction and ledger path as manifest files.

This milestone has no plugin-provided privileged actions or custom action API. Declarative resources use the host's existing ownership and elevation rules. New capabilities require a separate, versioned WIT world; they must not be added implicitly to the planner world.
