# Planner context

`plan` receives one `Context` and nothing else.

| Field | Meaning |
| --- | --- |
| `plugin_id` | the plugin binding's own identifier |
| `app_id` | application id |
| `app_name` | display name |
| `app_version` | application version |
| `install_directory` | resolved install root |
| `scope` | `Scope::User` or `Scope::Machine` |
| `target` | Zup target, such as `windows-x64` |
| `selected_components` | the component ids a person selected |

```rust
use zup_sdk::plugin::prelude::*;

if context.scope == Scope::Machine {
    // scope.as_str() is "user" or "machine"
}
```

`install_directory` is a template. Linux installers resolve its location variables to a literal root before invoking a plugin, consistently for installation and maintenance. Build-host previews can still contain location variables. `${install}` and the other location variables
are resolved by Zup as it writes each resource, which is why a generated file
declares `"${install}/notes.txt"` rather than a path.

Every field describes the installation. A plugin produces the same plan for the same context. Build validation and `zup plan` execute source plugins on the build host while preserving the requested target. Installers execute verified AOT content through the same planner, whether the content is embedded or acquired.

Plugins support user and machine scope on Windows and Linux. Their resources must satisfy the selected platform's capabilities. A Linux machine plugin can generate owned files and declare systemd services; it cannot bypass destination authorization or enable unsupported desktop integration. The privileged worker reconstructs its plan from pinned installer content and compares the authorized fingerprint before execution.

## No manifest parsing

Do not open or parse `zup.toml` from a plugin. The context already carries the
supported inputs. If an extension needs more information, that belongs in a
versioned plugin API rather than an implicit file dependency.

## No target-machine discovery

The planner does not receive arbitrary machine state. It declares resources from
explicit project and install facts. Detection and installation policy for
prerequisites belong in the prerequisite system, not in a plugin side channel.
