# Components

Components model choices that affect what the installer owns: optional documentation, language packs, toolchains, shell integration, or other independently selectable parts.

## Declare components

```toml
[[components]]
id = "core"
name = "Acme"
required = true

[[components]]
id = "docs"
name = "Offline documentation"
default = false

[[components]]
id = "shell"
name = "Shell integration"
default = true
```

A required component cannot be deselected. An optional component uses `default` to define the initial selection.

Attach resources with `component`:

```toml
[[files]]
source = "docs/**"
destination = "${install}/docs"
component = "docs"
```

## Dependencies

Use `requires` when one component cannot exist without another:

```toml
[[components]]
id = "designer-tools"
name = "Designer tools"
requires = ["core"]
```

Keep the dependency graph about ownership, not UI layout.

## Groups

Groups tell a preset which choices belong together and how prominent the group is.

```toml
[[component_groups]]
id = "toolchains"
label = "Toolchains"
prominence = "secondary"
selection = "defaulted"
```

Assign components with `group = "toolchains"`.

`prominence` is `auto`, `primary`, or `secondary`. `selection = "explicit"` requires at least one optional component in the group to be selected before installation can proceed.

Groups do not dictate a specific widget or screen. The selected [preset](/presets/) decides how to present them.

## Conditions

Use `when` for resources controlled by an expression over selected components:

```toml
when = 'component("shell") && !component("portable")'
```

Prefer a direct `component = "id"` when one component is enough.
