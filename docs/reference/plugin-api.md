# Plugin API

Depend on `zup-sdk` with the `plugin` feature:

```toml
[dependencies]
zup-sdk = { version = "0.1.0", features = ["plugin"] }
```

```rust
use zup_sdk::plugin::prelude::*;

impl Plugin for MyPlugin {
    fn plan(context: Context) -> Result<Plan, Error> {
        // Return declared resources.
    }
}

zup_sdk::plugin::export!(MyPlugin);
```

Bindings come from `crates/zup-plugin-abi/wit/zup-plugin.wit`, the canonical `zup:plugin@1.0.0` contract. Prototype changes may break this contract without changing its version; rebuild plugins against the current SDK.

## Context

`Context` contains `plugin_id`, `app_id`, `app_name`, `app_version`, `install_directory`, `scope`, `target`, and `selected_components`. Scope is `User` or `Machine`.

## Resources

Start with `Plan::new()` and add resources:

| Method | Constructor |
| --- | --- |
| `generated_file` | `GeneratedFile::new(destination, bytes)` or `GeneratedFile::text(destination, text)` |
| `launcher` | `Launcher::menu(name, target)` or `Launcher::desktop(name, target)` |
| `path_entry` | `Path::new(directory)` |
| `service` | `Service::new(id, name, binary, execution)` |
| `protocol` | `Protocol::new(scheme, executable)` |
| `file_association` | `FileAssociation::new(id, extension, executable)` |

Resources use `.with_arguments(...)` where supported. Launchers accept `.with_working_directory(...)`; services and file associations accept display names or descriptions. Keep registration IDs stable across application upgrades.

Services expose `.with_start(...)`, `.with_lifecycle(...)`, `.with_readiness(...)`, and `.with_required_files(...)`. Their defaults and platform restrictions match [manifest services](/guide/services). Plugins describe resources; the trusted host authorizes and executes them.

`Plan::validate()` checks output limits locally. The host always validates the returned plan. See [resource examples](/plugins/resources).

## Errors

Use `Error::new(code, message)`, `Error::unsupported(message)`, or `Error::invalid(message)`. Read them with `.code()` and `.message()`.

## Build

```sh
zup plugin build
```

This builds `wasm32-unknown-unknown` and writes `dist/<name>.wasm`. Options include `--project`, `--profile`, and `--output`.

Repository tests require `cargo xtask fixtures build` first. Tests reject missing or stale fixtures; no prebuilt Wasm fixture is checked in.
