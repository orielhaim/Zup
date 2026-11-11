# System integration

Declare system integration as resources. Add only what the application owns.

## Launchers

```toml
[[launchers]]
id = "main"
location = "menu"
name = "Acme"
target = "${install}/Acme.exe"

[[launchers]]
id = "desktop"
location = "desktop"
name = "Acme"
target = "${install}/Acme.exe"
component = "desktop-shortcut"
```

Locations are `menu` and `desktop`. Optional arguments and a working directory are in the manifest reference.

## PATH

```toml
[[path]]
id = "cli-1"
value = "${install}/bin"
```

Use PATH entries for command-line tools. Do not add an application directory to PATH merely because it contains an executable.

## Services

```toml
[[services]]
execution = { type = "windows_job", account = "local_service" }
id = "acme-agent"
display_name = "Acme Agent"
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

Launchers, URI handlers, MIME packages, and icons use freedesktop integration. Desktop launchers follow the configured XDG desktop directory; disabled directories are refused, and desktop trust approval may still be required.

PATH entries use per-application `environment.d` files. They reach processes inheriting the systemd user-manager environment, not every shell or existing process. Zup does not edit shell configuration.

Protocol arguments require exactly one `%1` placeholder. Protocols and file associations must use the same handler command. MIME and desktop integration require `update-mime-database` and `update-desktop-database`. Zup does not change default handlers.

See [services](/guide/services) for systemd configuration and lifecycle policies.
