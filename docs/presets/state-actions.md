# State and actions

A preset renders the current `Snapshot` and sends typed `Action` requests. Do not infer installer state from which button was last pressed.

## Snapshot

The snapshot is the complete state of the installation, published by the host:

| Area | Contains |
| --- | --- |
| `product` | name, publisher, version, description |
| `surface` | fresh install or maintenance, with its component options |
| `state` | options, running, blocked, success, failure, recovery |
| `operation` | install, upgrade, modify, repair, uninstall |
| `progress` | current operation progress |
| `plan` | what the current choices would change |
| `diagnostic` | user-facing failure or blocker information |
| `update` | update status when configured |
| `repair_drift` | resources a repair found changed |
| `launch` | the application launch target when available |

```rust
let Some(snapshot) = self.state.read(cx).snapshot() else {
    return div().child("Waiting for the installer…");
};

for component in snapshot.surface.components() {
    // component.id, component.name, component.required, component.selected
}
```

`session.state()` is a GPUI entity: observing it redraws the view when the host publishes. A snapshot is the whole state, so a preset that missed a message still renders correctly.

## Actions

`Action` expresses what a person asked for:

```rust
session.send(Action::SetComponent {
    component: component.id.clone(),
    selected: !component.selected,
});
session.send(Action::Install);
```

| Group | Actions |
| --- | --- |
| Choices | `SetScope`, `SetComponent`, `SetInstallDirectory`, `ResetInstallDirectory` |
| Lifecycle | `Install`, `Update`, `Modify`, `Repair` |
| Uninstall | `RequestUninstall`, `ConfirmUninstall`, `DismissUninstall` |
| Recovery | `Cancel`, `Retry` |
| Support | `OpenLog`, `CopyDiagnostics` |
| Finish | `Launch`, `Close` |

Actions are intent, not interaction. The host validates every action, so sending `Install` twice does not start two installations. Enable controls from the snapshot rather than keeping a lifecycle state machine.

## Component groups

Render component groups from the snapshot rather than re-reading `zup.toml`.