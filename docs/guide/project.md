# Project

A Zup project is rooted by `zup.toml`.

```toml
schema = 1

[app]
id = "com.acme.desktop"
name = "Acme"
version = "1.0.0"

[build.targets.windows-x64]
source = { directory = "dist" }

[install]
scope = "user"
```

| Section | Purpose |
| --- | --- |
| `schema` | manifest schema version |
| `[app]` | application identity (`id`, `name`, `version` required; plus `publisher`, `description`, `main`, `icon`) |
| `[build.targets.*]` | target profiles |
| `[install]` | install scope |

Everything else is optional. Unknown keys are rejected; keep the schema directive (`#:schema ...`) at the top so editors validate while you type. For exact fields and defaults, use the [manifest reference](/reference/manifest).
