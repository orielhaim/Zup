# Plugins

Zup plugins are planner-only WebAssembly components. The checked-in [`wit/zup-plugin.wit`](../wit/zup-plugin.wit) file is the contract; use or point to that file instead of maintaining a divergent copy.

## Contract

A component implements the `plugin` world's `zup:plugin/planner@1.0.0` interface and exports its `plan` function. It must have zero imports. The planner receives explicit application, install, scope, canonical target, and selected-component facts and returns typed installation resources or a typed `plugin-error`.

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
schema = 1

[app]
id = "com.acme.desktop"
name = "Acme"
version = "1.4.0"
main = "Acme.exe"

[build]

[build.targets.windows-x64]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist" }

[install]
scope = "user"

[install.directory]
user = "${location.user_data}/Acme"

[[components]]
id = "core"
name = "Core files"
required = true

[[components]]
id = "full"
name = "Full installation"
default = false

[[plugins]]
id = "configure"
source = "plugins/configure.component.wasm"
component = "full"
when = 'component("full")'
targets = ["windows-x64"]
```

`id` identifies the binding and `source` is a project-relative path. The optional
`component`, `when`, and `targets` fields attach the plugin's resources to a
component, gate it on a condition, and limit it to target profiles. A `when`
expression is built only from `component("<id>")`, `!`, `&&`, `||`, and
parentheses.

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

The plugin compiler is in the developer CLI; the runtime templates are in `zup-installer`. Build both, then let the toolchain command stage the runtime beside `zup`:

```text
cargo build -p zup --bin zup
cargo build -p zup-installer --no-default-features --features headless --bin zup-setup-headless
cargo xtask toolchain build
```

zup builds and tests the runtime.

`zup build` resolves and hashes the source, rejects a core module, validates zero
imports and the exact planner export and signature, AOT-compiles the component,
verifies the AOT output, and embeds it with its target, WIT, engine, size, and
digest metadata. The runtime template comes from the toolchain beside `zup` rather
than from `--runtime`.

## Runtime and safety

The installer runtime loads only the verified AOT component embedded in the bundle
and uses the Wasmtime runtime with Component Model support only. It provides no
WASI, environment, clock, filesystem, network, randomness, or UI imports, and it
never JITs a plugin: an AOT component is the only thing it can load.

AOT bytes are native-code artifacts and are trusted only to the same extent as the
containing Setup package; bundle hashes detect corruption, while release authenticity
comes from Authenticode/TUF. Bundle self-hashes do not prove publisher provenance.

The loader requires the package target, the requested target, and its own compile
target to be the same triple, and refuses an artifact whose Wasmtime version,
engine fingerprint, plugin API version, or AOT format version does not match. See
[architecture](architecture.md#target-binding) for the same check on the bootstrap
plan and the process protocol.

The sandbox disables nondeterministic and concurrent WebAssembly features and
limits each invocation to 100,000,000 fuel, a 250 ms deadline, a 1 MiB stack, 512
memory pages (32 MiB), four memories, four tables, 10,000 table elements, and eight
instances. A plan is limited to 4,096 resources and 8 MiB of output, and
generated-file bytes count toward that 8 MiB rather than getting a separate
per-file budget. One component is limited to 64 MiB of AOT bytes, and one bundle to
128 plugin components and 256 MiB of uncompressed AOT data.

Failures stay typed. A component reports the WIT's own `plugin-error`; the runtime
reports `Setup`, `Trap`, `FuelExhausted`, `MemoryLimit`, `Cancelled`, `Timeout`,
`OutputLimit`, `ResourceLimit`, `InvalidOutput`, and `Internal`. A returned
generated file is hashed and merged into the ordinary installation plan, so install,
ownership, repair, upgrade, and uninstall use the same transaction and ledger path
as manifest files.

There are no plugin-provided privileged actions or custom action API. Declarative
resources use the host's existing ownership and elevation rules. New capabilities
require a separate, versioned WIT world; they must not be added implicitly to the
planner world.
