# Planner context

The planner receives only facts needed to derive installation resources.

| Field | Meaning |
| --- | --- |
| `plugin-id` | current plugin binding ID |
| `app-id` | application ID |
| `app-name` | display name |
| `app-version` | application version |
| `install-directory` | resolved install root |
| `install-scope` | `user` or `machine` |
| `target` | canonical target triple |
| `selected-components` | selected component IDs |

A plugin should produce the same resource plan for the same context.

## No manifest parsing

Do not open or parse `zup.toml` from a plugin. The planner contract already carries the supported inputs. If a new extension needs additional information, that belongs in a versioned plugin API rather than an implicit file dependency.

## No target-machine discovery

The planner does not receive arbitrary machine state. It declares resources from explicit project/install facts. Detection and installation policy for prerequisites belong in the prerequisite system, not in a plugin side channel.
