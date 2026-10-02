# Shortcuts, PATH and file types

These declarations register the application with the machine. All of them are
listed by `zup plan` and the installer's "show what will change" view, and all of
them are taken back out by an uninstall.

Every one takes the same three optional keys as a [file mapping](/configure/files#conditions):
`component`, `when`, and `targets`.

## Launchers

A shortcut in the Start menu or on the desktop.

```toml
[[launchers]]
location = "menu"
name = "Acme"
target = "${install}/acme.exe"
arguments = ["--installed"]

[[launchers]]
location = "desktop"
name = "Acme"
target = "${install}/acme.exe"
```

| Key | Required | Meaning |
| --- | --- | --- |
| `location` | yes | `menu` or `desktop` |
| `name` | yes | The label shown |
| `target` | yes | A [path template](/configure/install#path-templates) for the executable |
| `arguments` | no | Default empty |
| `working_directory` | no | A template for the working directory |

`location = "menu"` writes to the user's Start menu under `user` scope and the
all-users Start menu under `machine` scope. `location = "desktop"` follows the
same rule.

## PATH

A directory added to the search path for the scope that owns it, so a command
line can find the program.

```toml
[[path]]
value = "${install}"
```

Only `value` is required. It takes `component` and `when`, so leaving a
component out leaves `PATH` alone.

## Services

A Windows service.

```toml
[[services]]
id = "acme-agent"
name = "Acme Agent"
display_name = "Acme Background Agent"
binary = "${install}/agent.exe"
arguments = ["--service"]
start = "automatic"
```

| Key | Required | Meaning |
| --- | --- | --- |
| `id` | yes | A stable identifier |
| `name` | yes | The service name |
| `display_name` | no | The name shown in Services |
| `binary` | yes | A path template for the executable |
| `arguments` | no | Default empty |
| `start` | yes | `automatic`, `manual` or `disabled` |

A machine-scope install needs elevation, so a service in a `user`-scope
application is worth thinking about before you declare it.

## Protocols

A URI scheme the application handles.

```toml
[[protocols]]
scheme = "acme"
executable = "${install}/acme.exe"
args = ["--open", "%1"]
```

`scheme` must be a valid URI scheme, and each scheme can be declared once. This
registers the handler; it does not claim to be the default handler for the
scheme. `args` default to empty, and `%1` is the conventional Windows
placeholder for the incoming URI.

Protocols take `when` and `targets` but not `component`.

## File associations

A file extension the application handles.

```toml
[[file_associations]]
extension = ".acme"
id = "acme-project"
description = "Acme project"
executable = "${install}/acme.exe"
```

| Key | Required | Meaning |
| --- | --- | --- |
| `extension` | yes | A leading dot, e.g. `.acme`. No path separators |
| `id` | yes | A stable identifier |
| `description` | no | The type name shown by Windows |
| `executable` | yes | A path template for the handler |

Like protocols, this registers the handler and does not set a default. Each
extension can be declared once. They take `when` and `targets` but not
`component`.

::: tip If two declarations collide, Zup does not pick one
A duplicate service id, protocol scheme, file-association id or extension is an
error, not a precedence rule. Zup will not guess which of two conflicting
declarations a person meant.
:::

Next: [components](/configure/components).
