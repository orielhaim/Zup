# Plugins

A plugin is a WebAssembly component that contributes resources to the
installation plan. Zup declares one, builds it, and calls it once while planning.

Use one when the contribution depends on something only the plugin knows: a file
whose name is derived from the target, a service that depends on which components
were selected.

If the resource is a fixed list, declare it in `zup.toml` instead. A plugin is
a build artifact you have to compile, ship and keep compatible.

## Declare it

```toml
[[plugins]]
id = "acme-integrations"
source = "plugins/acme-integrations.wasm"
```

| Key | Required | Meaning |
| --- | --- | --- |
| `id` | yes | A stable identifier |
| `source` | yes | A project-relative, `/`-separated path to the `.wasm` |
| `component` | no | Only called when that component is selected |
| `when` | no | Only called when a [condition](/configure/files#conditions) holds |
| `targets` | empty | Only included for these target profiles |

`source` may not be absolute, may not start with `/`, may not contain `\`, may
not have a drive prefix, and may not contain `..`. Ids are compared
case-insensitively, so `Helper` and `helper` collide.

The same `component` / `when` / `targets` triple applies to every other resource
declaration, and means the same thing: this runs when the person made that
choice.

## The interface

One function, in one interface, in one world:

```wit
package zup:plugin@1.0.0;

interface planner {
  plan: func(context: context) -> result<installation-plan, plugin-error>;
}

world plugin {
  export planner;
}
```

There is no `pre_install`, no `post_install`, no `uninstall`, no `validate`.

## What it receives

| Field | Type |
| --- | --- |
| `plugin-id` | `string` |
| `app-id` | `string` |
| `app-name` | `string` |
| `app-version` | `string` |
| `install-directory` | `string` |
| `install-scope` | `user` or `machine` |
| `target` | `string` |
| `selected-components` | `list<string>` |

That is the whole of the plugin's knowledge of the machine. It does not get the
plan, the payload, or the manifest.

## What it returns

A list of resource items. Six kinds, and no others:

```wit
record installation-plan { resources: list<resource-item> }

variant resource-item {
    generated-file(generated-file),
    launcher(launcher),
    path-entry(path-entry),
    service(service),
    protocol(protocol),
    file-association(file-association),
}
```

| Kind | Fields |
| --- | --- |
| `generated-file` | `destination`, `contents` |
| `launcher` | `location` (`menu` or `desktop`), `name`, `target`, `arguments`, `working-directory` |
| `path-entry` | `value` |
| `service` | `id`, `name`, `display-name`, `binary`, `arguments`, `start` |
| `protocol` | `scheme`, `executable`, `args` |
| `file-association` | `extension`, `id`, `description`, `executable` |

Returning `plugin-error` rejects the whole plan. There is no partial result.

## What it cannot do

`MAX_HOST_CALLS` is zero. A plugin cannot read a file, resolve a path, run a
process, get a random number, get the time, or see anything beyond the eight
context fields. There is no WASI.

Anything a plugin wants to write is returned as a `generated-file`'s `contents`.
Zup stores it in the build and the installer writes it at install time.

Floats, SIMD and threads are disabled in the Wasmtime configuration, so a
plugin's output is deterministic for a given input. That is a property of the
engine, not a convention.

## Limits

| Limit | Value |
| --- | --- |
| Fuel per call | 100,000,000 |
| Wall clock per call | 250 ms |
| Memory | 32 MiB across at most 4 memories |
| Plan resources | 4,096 |
| Encoded plan | 8 MiB |
| Generated file | 1 MiB |
| Error message | 512 bytes |
| Plugin source | 16 MiB |

Exceeding fuel, the deadline or memory is reported distinctly, so
`zup check` tells you which one:

```text
zup.check.plugin_compile_failed: plugin `acme-integrations` exhausted its fuel budget
```

## Building

Zup compiles the plugin ahead of time when it builds the installer, hashes it,
and records the Wasmtime version, the AOT format version, the plugin API version
and the WIT digest alongside it. Changing any sandbox limit invalidates cached
AOT artifacts rather than silently reusing one built under different rules.

A plugin that fails to compile fails `zup check` and `zup build`. Nothing is
written.

Next: [machine-readable output](/advanced/automation).
