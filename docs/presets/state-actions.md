# State and actions

A preset renders the current `UiSnapshot` and sends typed `UiAction` requests.

That is the public control model. Do not infer installer state from which button was last pressed.

## Snapshot

The snapshot exposes the information a window needs to render:

| Area | Contains |
| --- | --- |
| Product | name, publisher, version, description |
| Surface | fresh install or maintenance |
| Choices | scope, components, component groups, install directory |
| State | options, running, blocked, success, failure, recovery |
| Operation | install, upgrade, modify, repair, uninstall |
| Progress | current operation progress |
| Plan | what current choices would change |
| Diagnostic | user-facing failure/blocker information |
| Update | update status when configured |
| Launch | application launch target when available |

Observe `session.state()` and render from the latest snapshot.

## Actions

`UiAction` expresses user intent:

```rust
session.send(UiAction::Install);
```

Main action groups:

| Group | Actions |
| --- | --- |
| Choices | `SetScope`, `SetComponent`, `SetInstallDirectory`, `ResetInstallDirectory` |
| Lifecycle | `Install`, `Update`, `Modify`, `Repair` |
| Uninstall | `RequestUninstall`, `ConfirmUninstall`, `DismissUninstall` |
| Recovery | `Cancel`, `Retry` |
| Support | `OpenLog`, `CopyDiagnostics` |
| Finish | `Launch`, `Close` |

The installer validates an action against its current state. The preset should therefore enable controls from the snapshot instead of maintaining a separate lifecycle state machine.

## Component groups

Render component groups from the snapshot rather than re-reading `zup.toml`. Groups carry display metadata (`label`, `description`, `prominence`, `selection`) plus component IDs.

`prominence` is product intent, not a required layout. A preset may present primary and secondary groups differently as long as the choice remains clear.
