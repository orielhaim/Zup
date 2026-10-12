# Configure a plugin

Declare a built component in `zup.toml`:

```toml
[[plugins]]
id = "configure"
source = "plugins/configure.component.wasm"
```

`id` is the stable binding name. `source` is a project-relative path using `/` separators.

## Bind to a component

```toml
[[plugins]]
id = "shell-config"
source = "plugins/shell.component.wasm"
component = "shell"
```

The plugin's resources participate only when that component is selected.

## Use a condition

```toml
[[plugins]]
id = "integration-config"
source = "plugins/integration.component.wasm"
when = 'component("core") && component("shell")'
```

## Limit by target profile

```toml
[[plugins]]
id = "windows-config"
source = "plugins/windows.component.wasm"
targets = ["windows-x64", "windows-arm64"]
```

These are the same selectors used by manifest resources. See [Selection and conditions](/guide/selection).
