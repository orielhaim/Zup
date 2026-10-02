# Plugin API

Package:

```wit
package zup:plugin@1.0.0;
```

World:

```wit
world plugin {
  export planner;
}
```

A plugin exports one operation:

```wit
plan: func(context: context) -> result<installation-plan, plugin-error>;
```

## Context

```text
plugin-id: string
app-id: string
app-name: string
app-version: string
install-directory: string
install-scope: user | machine
target: string
selected-components: list<string>
```

## Resource variants

`installation-plan.resources` accepts:

### `generated-file`

```text
destination: string
contents: list<u8>
```

### `launcher`

```text
location: menu | desktop
name: string
target: string
arguments: list<string>
working-directory: option<string>
```

### `path-entry`

```text
value: string
```

### `service`

```text
id: string
name: string
display-name: option<string>
binary: string
arguments: list<string>
start: automatic | manual | disabled
```

### `protocol`

```text
scheme: string
executable: string
args: list<string>
```

### `file-association`

```text
extension: string
id: string
description: option<string>
executable: string
```

The checked-in `wit/zup-plugin.wit` file is the authoritative interface definition for the repository version you build against.
