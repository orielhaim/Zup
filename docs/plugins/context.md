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
| `target` | canonical target triple |
| `selected_components` | the component ids a person selected |

```rust
use zup_sdk::plugin::prelude::*;

if context.scope == Scope::Machine {
    // scope.as_str() is "user" or "machine"
}
```

`install_directory` is a template. `${install}` and the other location variables
are resolved by Zup as it writes each resource, which is why a generated file
declares `"${install}/notes.txt"` rather than a path.

Every field is a fact about the installation, not about the machine. There is no
way to ask what is already installed, what is running, or where anything else
lives.

A plugin should produce the same resource plan for the same context.

## No manifest parsing

Do not open or parse `zup.toml` from a plugin. The context already carries the
supported inputs. If an extension needs more information, that belongs in a
versioned plugin API rather than an implicit file dependency.

## No target-machine discovery

The planner does not receive arbitrary machine state. It declares resources from
explicit project and install facts. Detection and installation policy for
prerequisites belong in the prerequisite system, not in a plugin side channel.