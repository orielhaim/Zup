# Selection and conditions

Many declarations share three selectors: `component`, `when`, and `targets`.

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

The expression language is `component("id")`, `!`, `&&`, `||`, and parentheses. Prefer `component = "id"` over an equivalent one-term expression.

## Target profiles

Use `targets` for a real per-target difference:

```toml
targets = ["windows-x64", "windows-arm64"]
```

Values are target **profile IDs**, not target names.

Keep a resource unfiltered when it applies to every selected target.

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
