# Install scope and paths

`[install]` controls who owns the installation and where its default root resolves.

## Scope

```toml
[install]
scope = "user"
```

Valid scopes:

| Value | Meaning |
| --- | --- |
| `user` | Install for the signed-in user |
| `machine` | Install for the machine |
| `either` | Let the installer choose between user and machine scope |

Use `either` only when that choice is part of the product. A thin bootstrapper cannot use `either`; it needs a fixed scope before it can acquire the runtime.

## Default directories

```toml
[install]
scope = "either"
allow_directory_override = true

[install.directory]
user = "${location.user_data}/Acme"
machine = "${location.programs}/Acme"
```

Only configure the scope paths your project can use.

`allow_directory_override` lets the installer UI offer a custom location on a fresh installation. Existing installations keep their recorded location.

## Templates

Paths are templates with `${...}` placeholders. There is no nesting, escaping, or default-value syntax; a `$` that does not start a placeholder is literal.

| Placeholder | Resolves to |
| --- | --- |
| `${app.id}`, `${app.name}`, `${app.version}` | application identity |
| `${install}` | install root for the selected scope |
| `${location.programs}` | Programs directory |
| `${location.user_data}` | user application data |
| `${location.shared_data}` | machine-wide application data |
| `${location.menu}`, `${location.desktop}` | Start menu, desktop |

`${app.*}` and `${install}` resolve at build time. `${location.*}` resolves on the target machine. `${install}` cannot appear in `[install.directory]` itself.

Prefer `${install}` for payload destinations. Do not hard-code a Windows known-folder path when a location template expresses the intent.
