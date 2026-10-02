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

Paths are templates. Common values include:

```text
${install}
${app.id}
${app.name}
${app.version}
${location.user_data}
${location.shared_data}
${location.programs}
${location.menu}
${location.desktop}
```

`${install}` is the resolved install root for the current scope. Prefer it for payload destinations and resources tied to the installed application.

Platform backends resolve semantic locations to native paths. Do not hard-code a Windows known-folder path when a location template expresses the intent.
