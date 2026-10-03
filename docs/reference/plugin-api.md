# Plugin API

Plugin authors depend on `zup-sdk` with the `plugin` feature:

```toml
[dependencies]
zup-sdk = { version = "0.1.0", features = ["plugin"] }
```

```rust
use zup_sdk::plugin::prelude::*;
```

The canonical cross-language interface is the WIT at
`crates/zup-plugin-abi/wit/zup-plugin.wit`, which declares:

```wit
package zup:plugin@1.0.0;

world plugin {
  export planner;
}
```

A plugin exports one operation:

```wit
plan: func(context: context) -> result<installation-plan, plugin-error>;
```

The SDK generates its bindings from that WIT, so a project has no copy of it and
there is no vendored file to fall out of date.

## `Plugin`

```rust
impl Plugin for MyPlugin {
    fn plan(context: Context) -> Result<Plan, Error> {
        // ...
    }
}

zup_sdk::plugin::export!(MyPlugin);
```

`export!` writes the component's export symbols and converts the `Plan` into what
the ABI carries. Nothing else in a plugin crate mentions WebAssembly.

## `Context`

```text
plugin_id             String
app_id                String
app_name              String
app_version           String
install_directory     String
scope                 Scope
target                String
selected_components   Vec<String>
```

`Scope` is `User` or `Machine`, with `as_str()` giving `"user"` or `"machine"`.

## `Plan`

| Method | Purpose |
| --- | --- |
| `new()` | a plan that declares nothing |
| `generated_file(GeneratedFile)` | a file written during install |
| `launcher(Launcher)` | a start-menu or desktop entry |
| `path_entry(impl Into<Path>)` | a directory to add to the system `PATH` |
| `service(Service)` | a Windows service |
| `protocol(Protocol)` | a URL scheme to register |
| `file_association(FileAssociation)` | a file type to associate |
| `resource(ResourceItem)` | a resource the SDK has no constructor for |
| `resources()` | what this plan declares, in order |
| `is_empty()` | whether it declares nothing |
| `validate()` | refuse an oversized plan before it reaches a host |

## Resource constructors

### `GeneratedFile`

```rust
GeneratedFile::new(destination, contents: impl Into<Vec<u8>>)
GeneratedFile::text(destination, contents: impl AsRef<str>)
```

### `Launcher`

```rust
Launcher::menu(name, target)
Launcher::desktop(name, target)
    .with_arguments([...])
    .with_working_directory("...")
```

### `Path`

```rust
Path::new("...").as_str()
```

### `Service`

```rust
Service::new(id, name, binary)
    .with_display_name("...")
    .with_arguments([...])
    .with_start(ServiceStart::Automatic)
```

`ServiceStart` is `Automatic`, `Manual` or `Disabled`.

### `Protocol`

```rust
Protocol::new(scheme, executable).with_arguments([...])
```

### `FileAssociation`

```rust
FileAssociation::for_extension(extension, executable).with_description("...")
```

## `Error`

```rust
Error::new(code, message)
Error::unsupported(message)     // code "unsupported"
Error::invalid(message)         // code "invalid-context"
error.code()
error.message()
```

## The WIT records behind these names

| Rust | WIT |
| --- | --- |
| `GeneratedFile` | `generated-file { destination, contents }` |
| `Launcher` | `launcher { location, name, target, arguments, working-directory }` |
| `LauncherLocation` | `launcher-location { menu, desktop }` |
| `Path` | `path-entry { value }` |
| `Service` | `service { id, name, display-name, binary, arguments, start }` |
| `ServiceStart` | `service-start { automatic, manual, disabled }` |
| `Protocol` | `protocol { scheme, executable, args }` |
| `FileAssociation` | `file-association { extension, id, description, executable }` |
| `Scope` | `install-scope { user, machine }` |
| `Error` | `plugin-error { code, message }` |

## Building

```bash
zup plugin build
```

That compiles for `wasm32-unknown-unknown` and componentises the result into
`dist/<name>.wasm`. `--profile` picks the Cargo profile, `--output` the
destination, `--project` the project directory.

The ABI version an artifact must declare is `zup:plugin@1.0.0`, and the
repository's copy of the WIT is the authoritative definition for the version you
build against.