# Project

A Zup project is rooted by `zup.toml`. Keep application policy there; keep build output in the target source directories it references.

## Required sections

```toml
schema = 1

[app]
id = "com.acme.desktop"
name = "Acme"
version = "1.0.0"

[build.targets.windows]
target = "x86_64-pc-windows-msvc"
source = { directory = "dist" }

[install]
scope = "user"
```

The required top-level data is:

| Section | Purpose |
| --- | --- |
| `schema` | Manifest schema version |
| `[app]` | Application identity |
| `[build.targets.*]` | One or more target profiles |
| `[install]` | Install scope |

Everything else is optional.

## Application identity

```toml
[app]
id = "com.acme.desktop"
name = "Acme"
version = "1.4.0"
publisher = "Acme Labs"
description = "Desktop client for Acme."
main = "Acme.exe"
icon = "assets/icon.svg"
```

`id`, `name`, and `version` are required. `main` identifies the application executable when a feature needs the main program. `icon` can be a project path or an icon table with `source` and `padding`.

## Strict parsing

Unknown manifest keys are rejected. Keep the schema directive at the top of the file so editors can validate while you type:

```toml
#:schema https://zup.orielhaim.com/schema/zup.toml.json
```

For exact fields and defaults, use the [manifest reference](/reference/manifest). Do not copy the complete schema into normal guide pages.

## Format without losing comments

```bash
zup fmt
```

Use `zup check` after structural edits.
