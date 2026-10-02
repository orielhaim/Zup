# `zup.toml`

The manifest is strict: unknown fields are rejected.

## Top level

| Key | Required | Default | Purpose |
| --- | --- | --- | --- |
| `schema` | yes | - | schema version; current value `1` |
| `app` | yes | - | application identity |
| `frontend` | no | `gui` | `gui`, `console`, `headless` |
| `ui` | no | `{}` | preset package and settings |
| `build` | yes | - | target and artifact profiles |
| `install` | yes | - | scope and install root |
| `prerequisites` | no | `[]` | required software |
| `updates` | no | - | update repository/channel |
| `distribution` | no | - | client content host |
| `publish` | no | - | publishing targets |
| `components` | no | `[]` | selectable application parts |
| `component_groups` | no | `[]` | component presentation groups |
| `plugins` | no | `[]` | planner extensions |
| `files` | no | `[]` | payload mappings |
| `launchers` | no | `[]` | menu/desktop launchers |
| `path` | no | `[]` | PATH entries |
| `services` | no | `[]` | services |
| `protocols` | no | `[]` | URI protocols |
| `file_associations` | no | `[]` | file type registrations |

## `[app]`

| Key | Required | Meaning |
| --- | --- | --- |
| `id` | yes | stable application ID |
| `name` | yes | display name |
| `version` | yes | application version |
| `publisher` | no | publisher display name |
| `main` | no | main application executable/template |
| `description` | no | user-facing description |
| `icon` | no | icon path or `{ source, padding }` table |

## Frontends

Top-level `frontend` and per-target frontend values are:

- `gui`
- `console`
- `headless`

The default is `gui`.

## `[build.targets.<profile>]`

Every project needs at least one target profile.

| Key | Required | Meaning |
| --- | --- | --- |
| `target` | yes | canonical target triple |
| `source` | yes | `{ directory = "..." }` payload source |
| `frontend` | no | target-specific frontend override |
| `install` | no | target-specific install override |

## `[build.artifacts.<id>]`

Optional. With no declared artifacts, Zup builds one installer per selected target.

| Key | Required | Default | Meaning |
| --- | --- | --- | --- |
| `kind` | no | `universal` | `single` or `universal` |
| `mode` | no | `offline` | `offline` or `thin` |
| `targets` | yes | - | target profile IDs |
| `channel` | no | - | follow a release channel |
| `output` | no | derived | output file name |

## `[install]`

| Key | Required | Default | Meaning |
| --- | --- | --- | --- |
| `scope` | yes | - | `user`, `machine`, `either` |
| `directory` | no | empty | user/machine path templates |
| `allow_directory_override` | no | `false` | allow a custom path on fresh install |

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

| Key | Required | Meaning |
| --- | --- | --- |
| `preset` | no | project-relative `.zupui`; absent uses the bundled preset |
| `settings` | no | values validated by that preset's schema |

See [Presets](/presets/).

## Components

`[[components]]`:

| Key | Required | Default |
| --- | --- | --- |
| `id` | yes | - |
| `name` | yes | - |
| `description` | no | - |
| `required` | no | `false` |
| `default` | no | `true` |
| `requires` | no | `[]` |
| `group` | no | implicit group |
| `targets` | no | `[]` = all selected profiles |

`[[component_groups]]`:

| Key | Required | Default |
| --- | --- | --- |
| `id` | yes | - |
| `label` | no | - |
| `description` | no | - |
| `prominence` | no | `auto` |
| `selection` | no | `defaulted` |
| `targets` | no | `[]` |

`prominence`: `auto`, `primary`, `secondary`. `selection`: `defaulted`, `explicit`.

## Resources

### `[[files]]`

`source` and `destination` are required. Optional: `component`, `when`, `allow_empty` (default `false`), `targets`.

### `[[launchers]]`

Required: `location`, `name`, `target`. Optional: `arguments`, `working_directory`, `component`, `when`, `targets`. `location` is `menu` or `desktop`.

### `[[path]]`

Required: `value`. Optional: `component`, `when`, `targets`.

### `[[services]]`

Required: `id`, `name`, `binary`, `start`. Optional: `display_name`, `arguments`, `component`, `when`, `targets`. `start` is `automatic`, `manual`, or `disabled`.

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

`[[prerequisites]]` requires `id`, `name`, `requirement`, and `package`.

Optional fields: `description`, `component`, `when`, `target`, `installer`, `targets`.

Requirement kinds:

- `runtime` - `id`, optional `version`
- `installed_package` - `id`, optional `version`
- `file_version` - `path`, optional `version`

Package kinds:

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
