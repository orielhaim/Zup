# Install scope and paths

`[install]` controls who owns the installation and where its default root resolves.

## Scope

```toml
[install]
scope = "user"
```

Valid scopes:

| Value | Meaning |
| --- | --- |
| `user` | Install for the signed-in user |
| `machine` | Install for the machine |
| `either` | Let the installer choose between user and machine scope |

Use `either` when the person installing should choose the scope. Thin and offline
launchers collect that choice before acquisition. Automation must pass
`--scope user` or `--scope machine`; it never receives a guessed scope.

## Default directories

```toml
[install]
scope = "either"
allow_directory_override = true

[install.directory]
user = "${location.user_data}/Acme"
machine = "${location.programs}/Acme"
```

Only configure the scope paths your project can use.

`allow_directory_override` lets the installer UI offer a custom location on a fresh installation. Existing installations keep their recorded location.

## Templates

Paths are templates with `${...}` placeholders. There is no nesting, escaping, or default-value syntax; a `$` that does not start a placeholder is literal.

| Placeholder | Resolves to |
| --- | --- |
| `${app.id}`, `${app.name}`, `${app.version}` | application identity |
| `${install}` | install root for the selected scope |
| `${location.programs}` | Programs directory |
| `${location.user_data}` | user application data |
| `${location.shared_data}` | machine-wide application data |
| `${location.menu}`, `${location.desktop}` | Start menu, desktop |

`${app.*}` and `${install}` resolve at build time. `${location.*}` resolves on the target machine. `${install}` cannot appear in `[install.directory]` itself.

Prefer `${install}` for payload destinations. Do not hard-code a Windows known-folder path when a location template expresses the intent.

## Interrupted operations

Zup journals filesystem and system changes before reporting completion. A committed
transaction can still require ownership publication or cleanup. Until that work
succeeds, the installer reports pending reconciliation rather than success.

Recovery verifies persisted state before continuing. Missing, corrupt, inaccessible,
or misidentified journals stop the operation. Keep the state directory intact when
reporting a recovery failure. Deleting it discards ownership and rollback information.

Filesystem changes, services, and other system integrations do not commit atomically
together. Recovery reconciles recorded work with the machine. An ambiguous change
requires intervention rather than an unsafe overwrite or removal.
