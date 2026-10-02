# Selection and conditions

Many Zup declarations use the same three selectors:

- `component` - bind the declaration to one component;
- `when` - evaluate a boolean expression over selected components;
- `targets` - include the declaration only for named target profiles.

Learn the selectors once. Their meaning does not change between files, plugins, launchers, services, prerequisites, or other targeted resources.

## One component

Use `component` when one component owns the resource:

```toml
[[files]]
source = "docs/**"
destination = "${install}/docs"
component = "docs"
```

The resource participates only while `docs` is selected.

## Boolean conditions

Use `when` when the rule depends on more than one component:

```toml
when = 'component("core") && component("shell")'
```

The expression language contains:

```text
component("id")
!
&&
||
( )
```

Prefer `component = "id"` over an equivalent one-term expression.

## Target profiles

Use `targets` for a real per-target difference:

```toml
targets = ["windows-x64", "windows-arm64"]
```

Values are target **profile IDs**, not a second target-triple declaration.

Keep a resource unfiltered when it applies to every selected target. That makes the common path visible in the manifest and keeps target-specific branches small.

## Combine selectors

Selectors compose:

```toml
[[plugins]]
id = "shell-config"
source = "plugins/shell.component.wasm"
component = "shell"
targets = ["windows-x64"]
```

The plugin is included only for the `windows-x64` profile and only while `shell` is selected.
