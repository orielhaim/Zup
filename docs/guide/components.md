# Components

Components model independently selectable parts: optional documentation, language packs, shell integration.

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

A required component cannot be deselected. `default` sets the initial selection of an optional one.

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
