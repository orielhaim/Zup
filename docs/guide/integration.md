# System integration

Declare system integration as resources. Add only what the application owns.

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

Locations are `menu` and `desktop`. Optional arguments and a working directory are in the manifest reference.

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

File associations register a capability; they do not make the application the default handler.

## Linux

User-scope Linux targets lower the same resources into freedesktop integration (`menu` launchers, protocol handlers, MIME packages, icons). Protocol arguments must carry exactly one `%1` placeholder, and every protocol and file association must name the same handler command.

`desktop` launchers and `[[path]]` entries are refused on Linux, as is `mimeapps.list` takeover: registration is capability, never a default. Installing integration needs `update-mime-database` and `update-desktop-database` on the target machine; a project with no integration resources needs neither.
