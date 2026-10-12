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
