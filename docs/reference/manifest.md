# `zup.toml`

The manifest is strict: unknown fields are rejected.

## Top level

| Key                 | Required | Default | Purpose                           |
| ------------------- | -------- | ------- | --------------------------------- |
| `schema`            | yes      | -       | schema version; current value `1` |
| `app`               | yes      | -       | application identity              |
| `frontend`          | no       | `gui`   | `gui`, `console`, `headless`      |
| `ui`                | no       | `{}`    | preset package and settings       |
| `build`             | yes      | -       | target and artifact profiles      |
| `install`           | yes      | -       | scope and install root            |
| `prerequisites`     | no       | `[]`    | required software                 |
| `updates`           | no       | -       | update repository/channel         |
| `distribution`      | no       | -       | client content host               |
| `publish`           | no       | -       | publishing targets                |
| `components`        | no       | `[]`    | selectable application parts      |
| `component_groups`  | no       | `[]`    | component presentation groups     |
| `plugins`           | no       | `[]`    | planner extensions                |
| `files`             | no       | `[]`    | payload mappings                  |
| `launchers`         | no       | `[]`    | menu/desktop launchers            |
| `path`              | no       | `[]`    | PATH entries                      |
| `services`          | no       | `[]`    | services                          |
| `protocols`         | no       | `[]`    | URI protocols                     |
| `file_associations` | no       | `[]`    | file type registrations           |

## `[app]`

| Key           | Required | Meaning                                  |
| ------------- | -------- | ---------------------------------------- |
| `id`          | yes      | stable application ID                    |
| `name`        | yes      | display name                             |
| `version`     | yes      | application version                      |
| `publisher`   | no       | publisher display name                   |
| `main`        | no       | logical reference to the main executable |
| `description` | no       | user-facing description                  |
| `icon`        | no       | icon path or `{ source, padding }` table |

## Frontends

Top-level `frontend` and per-target frontend values are:

- `gui`
- `console`
- `headless`

The default is `gui`. A Linux `gui` installer presents the same Default Preset
through the shared session controller; at run time it needs a Wayland
(`WAYLAND_DISPLAY`) or X11 (`DISPLAY`) session, and refuses with a diagnostic
otherwise.

## `[build.targets.<profile>]`

Every project needs at least one target profile.

| Key        | Required                                           | Meaning                                |
| ---------- | -------------------------------------------------- | -------------------------------------- |
| `target`   | inferred from the profile name when it is a target | `windows-x64`, `linux-x64`, ...        |
| `source`   | yes                                                | `{ directory = "..." }` payload source |
| `frontend` | no                                                 | target-specific frontend override      |
| `install`  | no                                                 | target-specific install override       |

A profile named `windows-x64` needs no `target` field. A profile with any
other name, such as `production`, sets `target = "windows-x64"`.

## `[build.artifacts.<id>]`

Optional. With no declared artifacts, Zup builds one installer per selected target.

| Key       | Required | Default     | Meaning                  |
| --------- | -------- | ----------- | ------------------------ |
| `kind`    | no       | `universal` | `single` or `universal`  |
| `mode`    | no       | `offline`   | `offline` or `thin`      |
| `targets` | yes      | -           | target profile IDs       |
| `channel` | no       | -           | follow a release channel |
| `output`  | no       | derived     | output file name         |

## `[install]`

`data` is an optional array of persistent-directory declarations. Each declaration
has a stable `id`, exactly one of `path` or `service`, and an optional
`compatibility` contract. Path placement defaults to owner-only access. Machine
paths can declare `access = { kind = "services", services = ["worker"] }`.
Service placement delegates state provisioning to the native service mechanism;
its writer is the named service. Empty or duplicate writer lists are invalid.
The default `{ kind = "stable" }` declares release-independent data. A schema
contract has `{ kind = "schema", writes = N, reads = [N, ...] }`, with positive
schema numbers. See [Persistent data](../guide/payload.md) for ownership and
rollback behavior.

| Key                        | Required | Default | Meaning                              |
| -------------------------- | -------- | ------- | ------------------------------------ |
| `scope`                    | yes      | -       | `user`, `machine`, `either`          |
| `directory`                | no       | empty   | user/machine path templates          |
| `allow_directory_override` | no       | `false` | allow a custom path on fresh install |

Example:

```toml
[install]
scope = "either"
allow_directory_override = true

[install.directory]
user = "${location.user_data}/Acme"
machine = "${location.programs}/Acme"
```

## `[ui]`

| Key        | Required | Meaning                                                   |
| ---------- | -------- | --------------------------------------------------------- |
| `preset`   | no       | project-relative `.zupui`; absent uses the bundled preset |
| `settings` | no       | values validated by that preset's schema                  |

See [Presets](/presets/).

## Components

`[[components]]`:

| Key           | Required | Default                      |
| ------------- | -------- | ---------------------------- |
| `id`          | yes      | -                            |
| `name`        | yes      | -                            |
| `description` | no       | -                            |
| `required`    | no       | `false`                      |
| `default`     | no       | `true`                       |
| `requires`    | no       | `[]`                         |
| `group`       | no       | implicit group               |
| `targets`     | no       | `[]` = all selected profiles |

`[[component_groups]]`:

| Key           | Required | Default     |
| ------------- | -------- | ----------- |
| `id`          | yes      | -           |
| `label`       | no       | -           |
| `description` | no       | -           |
| `prominence`  | no       | `auto`      |
| `selection`   | no       | `defaulted` |
| `targets`     | no       | `[]`        |

`prominence`: `auto`, `primary`, `secondary`. `selection`: `defaulted`, `explicit`.

## Resources

### `[[files]]`

`id` and `source` are required. `id` is a stable logical name for the mapping;
`source` is a pattern within the target's source directory. Optional fields are
`placement`, `component`, `when`, `allow_empty`, `executable`, and `targets`.
Both boolean fields default to `false`.

With no `placement`, matched files belong to an immutable generation and retain
their relative paths. For example:

```toml
[[files]]
id = "application"
source = "**/*"
```

Directory mappings append each matched relative path. Use
`placement = { kind = "generation", directory = "lib" }` to place a tree below
`lib`, or `placement = { kind = "fixed", directory = "${install}/shared" }`
when the files require that physical directory.

Single-file mappings require exactly one source match and name its destination
explicitly. Their logical identity remains the mapping's `id` even if the source
file is renamed:

```toml
[[files]]
id = "program"
source = "release/acme-linux"
placement = { kind = "generation_file", path = "bin/acme" }
```

Use `fixed_file` with a path template for a physically fixed single file.
Placement never changes automatically after an activation failure.

Fixed placement defaults to `replacement = "atomic"`. The public file must stay
present while its owned instance is replaced. `replacement = "quiescent"` permits
a temporary absence only within a verified runtime boundary. Declaring this
policy does not establish that boundary. Zup refuses replacement when the native
filesystem or runtime coordination cannot meet the declared requirement.

`app.main = "bin/acme"` refers to that member of the `application` tree mapping.
For a single-file mapping, use
`main = { kind = "file", declaration = "program" }` in `[app]`.
Other tree mappings use
`main = { kind = "tree_member", declaration = "tools", member = "bin/acme" }`.
The main reference must select a shipped payload. It automatically grants that
payload executable intent; `executable = true` marks additional executables.

`executable` states that the shipped file is meant to be runnable. It is an
intent, not a permission: the platform decides how to satisfy it. It is never
inferred from the file's contents, and there is no way to author a raw mode —
a build machine's mode bits are not portable truth.

### `[[launchers]]`

Required: `id`, `location`, `name`, `target`. Keep `id` stable when the display name or executable path changes. Optional: `arguments`, `working_directory`, `component`, `when`, `targets`. `location` is `menu` or `desktop`.

### `[[path]]`

Required: `id`, `value`. `id` is a stable logical declaration name; it remains unchanged when a generation directory changes. Optional: `component`, `when`, `targets`.

### `[[services]]`

Required: `id`, `binary`, `start`, `execution`. Optional: `native_name`, `display_name`, `arguments`, `lifecycle`, `readiness`, `requires_files`, `component`, `when`, `targets`. `start` is `automatic`, `manual`, or `disabled` and controls boot policy independently of installation-time execution.

The default native name is scoped to the application and logical service ID.
Use `native_name` for a hard-coded native contract, including the `.service` suffix
for systemd. `display_name` is a presentation label and defaults to the logical ID.
A whole `"${service.worker}"` argument resolves the native name of declared service
`worker`; `"${job.worker}"` supplies its Windows job locator.

`execution` is explicit owned configuration. Windows accepts `{ type = "windows_job", account = "local_service" }`, `network_service`, or an explicitly selected `local_system`. Managed Windows registrations use an unrestricted service SID. Credential-bearing accounts are not supported. systemd accepts `{ type = "systemd_dynamic_user" }` or `{ type = "systemd_user", user = "account", group = "optional-group" }`. Named accounts must already exist; numeric identities and account provisioning are not supported. Generated units require `NoNewPrivileges=yes`.

`lifecycle = { install = "stopped", change = "preserve" }` is the default. Set `install = "running"` to request startup after a new registration. `change = "preserve"` restores a previously running service after its executable or registration changes and leaves a stopped service stopped. `change = "stopped"` leaves it stopped after a change. Disabled services cannot request startup.

See [service lifecycle](/guide/services) for the implemented behavior and current limits.

`lifecycle.timeouts` is an optional manager-specific policy with `type = "systemd"`, `"openrc"`, or `"windows"`, matching `execution`. Omission resolves the manager defaults during planning. See [timeout policies](/guide/services#policies) for the fields, native escalation, observation bounds, and compensation behavior. Windows settings bound observation only; SCM does not expose per-service termination deadlines.

`readiness` defaults to `manager`. Linux systemd also supports `notify`, requiring the main service process to send `READY=1`. Windows rejects `notify`. Readiness is owned configuration and changing it requires a registration update.

### `[[protocols]]`

Required: `scheme`, `executable`. Optional: `args`, `when`, `targets`.

### `[[file_associations]]`

Required: `extension`, `id`, `executable`. Optional: `description`, `when`, `targets`.

## Plugins

`[[plugins]]` requires `id` and `source`. Optional: `component`, `when`, `targets`.

See [Plugins](/plugins/).

## Conditions

A condition is a boolean component expression:

```toml
when = 'component("docs") && !component("minimal")'
```

Operators: `!`, `&&`, `||`, parentheses.

## Prerequisites

`[[prerequisites]]` requires `id`, `name`, `requirement`, and `package`. Linux uses `requirement = { kind = "native_package", id = "package-name", minimum_version = "native-version" }` with `package = { type = "detect_only" }` or `{ type = "repository" }`. The minimum version is optional and uses native package-manager ordering. See [Prerequisites](/guide/prerequisites).

Optional fields: `description`, `component`, `when`, `target`, `installer`, `targets`.

Requirement kinds:

- `native_package` - `id`, optional `minimum_version` (Linux native version)
- `runtime` - `id`, optional `version`
- `installed_package` - `id`, optional `version`
- `file_version` - `path`, optional `version`

Package kinds:

- `detect_only` - detect without installing
- `repository` - install through the native Linux package manager
- `embedded` - `path`, `sha256`, `size`
- `remote` - `url`, `sha256`, `filename`, optional `size`

Installer options include `arguments`, `success_exit_codes`, `reboot_exit_codes`, and `privilege` (`user` or `system`).

## Updates and distribution

`[updates]` requires:

- `repository`
- `channel`
- `root`

`[distribution]` accepts `host = "static" | "github"` and optional `channel`.

## `[publish.github]`

Required: `repository`.

Optional release policy includes workflow configuration, tag policy, release notes, `draft`, `prerelease`, `create_tag`, and `replace_conflicts`.

Use [GitHub Releases](/ship/github) for the normal workflow. Use the JSON Schema for nested GitHub workflow fields rather than copying those options into unrelated guide pages.
