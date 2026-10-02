# Plugin resources

A plugin returns an `installation-plan` containing resource items. Zup merges them into the ordinary plan.

## Generated file

```wit
record generated-file {
  destination: string,
  contents: list<u8>,
}
```

Use this for small configuration or metadata files derived by the plugin.

## Launcher

A launcher can target `menu` or `desktop` and includes name, target, arguments and optional working directory.

## PATH entry

A path entry contributes one value to the install scope's search path.

## Service

A service declares ID, name, optional display name, binary, arguments and start policy (`automatic`, `manual`, `disabled`).

## Protocol

A protocol declares a URI scheme, executable and arguments.

## File association

A file association declares extension, ID, optional description and executable.

## Ownership

Return only resources the application should own. Once accepted, plugin resources follow the same lifecycle as equivalent manifest resources: they are part of planning, installation, repair, modification, upgrade and uninstall.

Do not return a fixed resource from a plugin merely to avoid writing it in the manifest. Static declarations are easier to read and audit in `zup.toml`.
