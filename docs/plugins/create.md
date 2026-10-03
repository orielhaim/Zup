# Create a plugin

Generate a plugin project:

```bash
zup plugin init configure
cd configure
```

That writes three files:

```text
configure/
  Cargo.toml
  src/lib.rs
  .gitignore
```

The manifest declares the target and the dependency, and nothing else:

```toml
[package]
name = "configure"
version = "0.1.0"
edition = "2024"
publish = false

# A plugin is Wasm, so it is a library that becomes a component rather than an
# executable.
[lib]
crate-type = ["cdylib"]

[dependencies]
zup-sdk = { version = "0.1.0", features = ["plugin"] }
```

The generated project is the whole of the scaffolding. The SDK generates its
bindings against the WIT Zup owns, and `zup plugin build` owns compiling for
`wasm32-unknown-unknown` and componentising the result, so there is nothing to
install, configure or keep in step.

## Write the plugin

```rust
use zup_sdk::plugin::prelude::*;

struct Configure;

impl Plugin for Configure {
    fn plan(context: Context) -> Result<Plan, Error> {
        Ok(Plan::new().generated_file(GeneratedFile::text(
            "${install}/configure.txt",
            format!("installing {} for {}", context.app_name, context.plugin_id),
        )))
    }
}

zup_sdk::plugin::export!(Configure);
```

That is a complete plugin. `plan` receives the [context](./context) and returns a
[plan](./resources); `export!` turns it into the component Zup loads.

Returning an `Error` is a normal answer, not a failure. It says "this plugin
cannot plan for that context", with a code a host can branch on and a message a
person can read:

```rust
if context.target.ends_with("-windows-msvc") {
    return Err(Error::unsupported("configure only applies to Windows"));
}
```

## Build it

```bash
zup plugin build
```

That is the whole command. It runs Cargo for `wasm32-unknown-unknown`, turns the
resulting core module into a component that implements the plugin world, and
writes `dist/configure.wasm`.

| Flag | Meaning |
| --- | --- |
| `--project <dir>` | Build a project somewhere else. Defaults to the current directory |
| `--profile <p>` | Cargo profile. Defaults to `release` |
| `--output <file>` | Where to write the component. Defaults to `dist/<name>.wasm` |

Zup asks Cargo where its output actually is rather than guessing a path, because
a guessed path is right until someone sets a target directory. The component
comes from the WIT Zup owns, so there is no second copy to disagree.

## Bind it to an application

```toml
[[plugins]]
id = "configure"
source = "plugins/configure.wasm"
```

Then validate:

```bash
zup check
```

Next: [Configure a plugin](./configure) for `component`, `when` and `targets`.