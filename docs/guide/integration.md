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

## Selection

Integration resources use the common selection fields supported by their type. See [Selection and conditions](./selection), then use the [manifest reference](/reference/manifest#resources) for the exact fields on each resource.
