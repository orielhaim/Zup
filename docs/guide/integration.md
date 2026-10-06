# System integration

Zup declares system integration as resources. Add only the resources the application owns.

## Launchers

```toml
[[launchers]]
location = "menu"
name = "Acme"
target = "${install}/Acme.exe"

[[launchers]]
location = "desktop"
name = "Acme"
target = "${install}/Acme.exe"
component = "desktop-shortcut"
```

Locations are `menu` and `desktop`. Optional arguments and a working directory are available in the manifest reference.

## PATH

```toml
[[path]]
value = "${install}/bin"
```

Use PATH entries for command-line tools. Do not add an application directory to PATH merely because it contains an executable.

## Services

```toml
[[services]]
id = "acme-agent"
name = "Acme Agent"
binary = "${install}/acme-agent.exe"
start = "automatic"
```

Start policy is `automatic`, `manual`, or `disabled`.

## URI protocols

```toml
[[protocols]]
scheme = "acme"
executable = "${install}/Acme.exe"
args = ["%1"]
```

## File associations

```toml
[[file_associations]]
extension = ".acme"
id = "Acme.Document"
description = "Acme document"
executable = "${install}/Acme.exe"
```

File associations register an application capability; they do not force the application to become the user's default handler.

## Linux

User-scope Linux targets lower the same resources into freedesktop integration:

| Manifest resource | Linux result |
| --- | --- |
| `menu` launcher | `$XDG_DATA_HOME/applications/<app-id>.desktop` |
| `[[protocols]]` | hidden handler entry advertising `x-scheme-handler/<scheme>`, URI delivered through `%u` |
| `[[file_associations]]` | `$XDG_DATA_HOME/mime/packages/<app-id>.xml` plus a hidden handler entry advertising the generated MIME type, files delivered through `%f` |
| `app.icon` | `$XDG_DATA_HOME/icons/hicolor/...` |

Protocol arguments must carry exactly one `%1` placeholder. Every protocol
must name the same handler command, as must every file association: one
hidden entry dispatches each kind.

Installing MIME or desktop integration needs `update-mime-database` and
`update-desktop-database` on the target machine. The installer checks before
changing anything; a project with no integration resources needs neither
tool. The shared database caches are regenerated, never owned: uninstall
removes only Zup's own sources and refreshes again.

Still refused on Linux: `desktop` launchers (no desktop-neutral way to place
a trusted desktop icon), `[[path]]` directory entries (a PATH mutation is
not command exposure, and shell configuration files are not edited), and
`mimeapps.list` takeover (registration is capability, never a default).

## Selection

Integration resources use the common selection fields supported by their type. See [Selection and conditions](./selection), then use the [manifest reference](/reference/manifest#resources) for the exact fields on each resource.
