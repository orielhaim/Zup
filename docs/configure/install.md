# Install location

`[install]` answers two questions: who the application installs for, and where it
goes.

```toml
[install]
scope = "user"
allow_directory_override = true

[install.directory]
user = "${location.user_data}/acme"
```

## scope

| Value | Meaning |
| --- | --- |
| `user` | The current user. No elevation |
| `machine` | Everyone on the machine. Requires elevation |
| `either` | The user chooses in the window |

`either` is the value that makes the installer show a scope question. It is also
the value `zup publish stage --thin` refuses, because a thin installer's
embedded trust context has to name one scope.

`--scope` overrides this on `zup plan`. A per-machine profile inherits the
top-level `[install]` unless it declares its own.

## directory

A path template per scope. Declare one for every scope your `scope` allows:
`user` for `user` and `either`, `machine` for `machine` and `either`. A missing
one is refused with `install directory for scope ... is not specified`.

## Path templates

A template is text with `${...}` placeholders. There is no nesting, no escaping
and no default-value syntax. A `$` that does not start a placeholder is literal,
so `"price: $5"` is fine.

| Placeholder | Resolves to |
| --- | --- |
| `${app.id}` | The application identifier |
| `${app.name}` | The application display name |
| `${app.version}` | The application version |
| `${install}` | The install directory for the selected scope |
| `${location.programs}` | The Programs directory |
| `${location.user_data}` | The user's application data directory |
| `${location.shared_data}` | The machine-wide application data directory |
| `${location.menu}` | The Start menu directory |
| `${location.desktop}` | The desktop directory |

`${app.*}` and `${install}` are resolved when the plan is built. The
`${location.*}` family is resolved on the target machine, so the same manifest
produces the right absolute path for whichever user runs the installer.

An unknown placeholder is an error: `unknown template variable ...`.

### Where the locations point

| Location | `user` scope | `machine` scope |
| --- | --- | --- |
| `programs` | `%ProgramFiles%` | `%ProgramFiles%` |
| `user_data` | the user's local application data | the user's local application data |
| `shared_data` | `%ProgramData%` | `%ProgramData%` |
| `menu` | the user's Start menu | the all-users Start menu |
| `desktop` | the user's desktop | the public desktop |

`programs` and `user_data` do not change with scope. That is deliberate: a
per-user application belongs in the user's own profile rather than in a
Program Files directory it cannot write.

`${location.user_data}` is the usual answer for `user` scope, and
`${location.programs}` for `machine`.

### The `${install}` placeholder

`${install}` expands to the install directory. Use it for payload files so a
change of location moves the files with it:

```toml
[[files]]
source = "acme.exe"
destination = "${install}"
```

It cannot appear in `[install.directory]` itself - that would be circular, and
Zup refuses it with `install directory for scope ... must not reference
${install}`.

## allow_directory_override

```toml
[install]
scope = "either"
allow_directory_override = true
```

Off by default. When it is on, the installer window offers a location field the
person can change. When it is off, the location is the one you declared.

`--install-directory` overrides it from the command line, for the scopes the
target actually installs to.

Next: [payload files](/configure/files).
