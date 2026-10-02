# Payload files

`[[files]]` maps files from a target's source directory into the installation.

```toml
[[files]]
source = "Acme.exe"
destination = "${install}"

[[files]]
source = "assets/**"
destination = "${install}/assets"
```

`source` is evaluated inside the selected target's `source.directory`. `destination` is the destination directory; matched files keep their file name and relative structure.

## Empty patterns

A pattern that matches nothing is an error by default. Make an intentionally optional pattern explicit:

```toml
[[files]]
source = "extras/**"
destination = "${install}/extras"
allow_empty = true
```

## Components

Bind files to a component when they should exist only when that component is selected:

```toml
[[files]]
source = "docs/**"
destination = "${install}/docs"
component = "docs"
```

See [Components](./components) for dependencies and groups.

## Selection

Files support the common `component`, `when`, and `targets` selectors. Use [Selection and conditions](./selection) for their shared semantics.
