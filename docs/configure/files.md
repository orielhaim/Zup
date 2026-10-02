# Payload files

`[[files]]` copies files out of a target's source directory and into the
installation.

```toml
[[files]]
source = "acme.exe"
destination = "${install}"

[[files]]
source = "runtime/**"
destination = "${install}/runtime"
```

| Key | Required | Meaning |
| --- | --- | --- |
| `source` | yes | A glob pattern inside the target's `source.directory` |
| `destination` | yes | A [path template](/configure/install#path-templates) naming a **directory** |
| `component` | no | Only write this when that [component](/configure/components) is selected |
| `when` | no | Only write this when a [condition](#conditions) holds |
| `allow_empty` | no | Accept a pattern that matches nothing. Default `false` |
| `targets` | no | Only apply to these target profiles |

## destination is a directory

`destination` names a directory, not a file path. The installer keeps each
matched file's own name inside it. `"${install}"` writes `acme.exe` into the
install directory, not into a directory called `acme.exe`.

To place a file under a different name, name the file directly:

```toml
[[files]]
source = "build/acme.exe"
destination = "${install}/Acme.exe"
```

A static prefix is stripped before the remainder is appended. With
`source = "runtime/**"` and `destination = "${install}/runtime"`, a file at
`runtime/lib/thing.dll` lands at `<install>/runtime/lib/thing.dll`.

## Patterns

Globset syntax with `/` as the path separator: `*`, `?`, `[...]`, `{a,b}`. `*`
does not cross a `/`. A pattern may not be empty, start with `/`, or contain
`..`.

A pattern that matches zero files is an error unless `allow_empty = true`:

```toml
[[files]]
source = "docs/**"
destination = "${install}/docs"
component = "docs"
allow_empty = true
```

Reach for `allow_empty` when the directory legitimately might not exist yet -
an empty docs tree is not a broken build.

## Conditions

`when` is a boolean expression over component selection.

| Form | Meaning |
| --- | --- |
| `component("id")` | That component is selected |
| `!expr` | Not |
| `a && b` | And |
| `a \|\| b` | Or |
| `(expr)` | Grouping |

Precedence is `!`, then `&&`, then `||`. A component id is a double-quoted
string with no escape processing.

```toml
[[files]]
source = "tools/**"
destination = "${install}/tools"
when = "component(\"debug\") || component(\"symbols\")"
```

`when` is evaluated during planning, against the components the person actually
selected. It is not evaluated when the manifest is parsed, so a `when` that
references a component the user cannot select is legal - it is never true.

Every resource that takes a `when` also takes `component`. Use `component` when
the resource is "part of" a feature, and `when` when it is "included if any of
these features is on".

## Restricting to targets

Any resource can name the target profiles it applies to:

```toml
[[files]]
source = "acme.exe"
destination = "${install}"
targets = ["x64"]
```

An empty list means every profile. A name that does not match a declared profile
is an error, and the diagnostic says which resource referenced it.

Next: [shortcuts, PATH and file types](/configure/registration).
