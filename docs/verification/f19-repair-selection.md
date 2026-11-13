# Repair selection and content

## Finding and reproduction

F19 in `Zup_PR17_Deep_Review.html` reports a session API failure before backend execution, not a native repair ownership failure. At e718968 the session runner translated the maintenance surface into explicit component overrides for Repair. For declared components `core` (required), `optional` (default on), and `extra` (default off), an installed selection `[core, extra]` became `enable=[core, extra], disable=[optional]`. The runtime refused that input. The surface directory could cause the same refusal. This concrete pre-edit trace follows `Selection::from_surface`, `selection_of`, `drive_operation`, and `prepare_operation`.

The graph lifecycle path independently acquired All, then replaced its acquisition plan with `repair_closure`. That filter silently discarded unknown digests and omitted plugin inputs. It ran after the download barrier.

## Contract

Repair uses the validated installed record. `plan_request` seeds installed components, preserves disabled defaults, and verifies exact resolved membership. Missing IDs, dependency expansion into disabled components, and newly required components refuse planning. Explicit enable, disable, or directory edits are refused, including before graph acquisition. Force changes drift handling, not selection authority.

The session runner treats Repair as an operation without editable selection. The maintenance surface remains presentation state. Modify and Upgrade retain selection controls.

Graph and terminal acquisition use the same `plan_request` and `select_components` resolver as runtime planning. `ComponentSelection::Only` now accepts `ComponentId` values directly. The string-list reconstruction and the `repair_closure` API, including acquire's optional digest argument, were removed. Acquisition and staging retain the barrier's original plan.

`component_closure` selects payload blobs by component and includes prerequisite artifacts, UI assets, preset, and plugin inputs. The barrier adds the native runtime even when payloads are cached. Required catalog entries are errors when absent; they are never filtered out. Generated files are recreated by the existing plugin planner from authenticated plugin inputs. This is a sufficient selected-content closure, not a drift-only download set. Verified cache reuse remains available.

Source selection still pins Repair to the installed release identity. Manifest/release/catalog checks, variant checks, handoff source verification, payload verification, worker authorization, and receipt binding remain in their existing layers. Embedded carriers keep their existing source custody. `plan_lifecycle` independently refuses a different component set or location, then checks deployment requirements and exact owned-resource repair semantics. Generation rebinding retains its existing narrow authority.

Linux uses the shared runtime/session and lifecycle planners; its update acquisition path is an Upgrade path. Windows graph, terminal, and acquired handoff paths use the selected closure. These source traces do not establish full frontend or elevation test coverage.

## Verification scope

The runtime regression covers selected-but-not-default, disabled-but-default, explicit Repair overrides, removed IDs, and dependency expansion. The session regression runs both force settings with conflicting surface components and location through shared preparation to backend completion. Its backend is a test host.

The Windows native regression writes and damages real generation files, verifies repaired bytes and identity, and compares committed component selection with the retained transaction plan. It tests the native commit boundary separately from session translation. The repair-binding regression refuses a changed selection while retaining checks for owned generation references and service policy.

Full embedded/online/handoff/GUI end-to-end equivalence, P3 generated-plugin repair, machine UAC repair, and exhaustive crash/sidecar/source matrices are not established by these regressions. No new claim is made for HTTP/2/TLS, disk-full, GUI automation, or power-loss testing. Existing F07-F18 contracts were not redesigned.

## Executed checks

- Windows: `cargo nextest run -p zup-runtime -p zup-session -p zup-update -p zup-plan -p zup-windows -p zup-installer --no-fail-fast`: 529 passed, 24 skipped.
- Windows session controller with `--features test-support`: 14 passed, including the F19 regression.
- Final runtime/session check after policy refactoring: 29 passed.
- Ubuntu WSL: `cargo nextest run -p zup-linux -p zup-runtime -p zup-session --no-fail-fast`: 376 passed, 19 skipped. Build jobs were limited to two and the target directory was on the Linux filesystem. Native tests use their isolated filesystem fixtures.
- `cargo fmt --all`, workspace all-target/all-feature Clippy with `-D warnings`, and workspace doctests passed. Cargo emitted existing manifest/dependency warnings separately from Clippy diagnostics.

The first Linux attempt used a temporary target directory and was interrupted by an apparent WSL shutdown. Its temporary artifacts disappeared; it provides no test evidence. Read-only diagnostics reported unclean journal files. No host/VM repair was attempted.

Skipped cases include elevated Windows coverage. No UAC prompt or machine installation was exercised in this run. Full interactive GUI/preset snapshots and P3 plugin repair remain unverified. Dependency changes that alter the resolved selection refuse planning; declaration-only semantic differences rely on the existing authenticated release pin and deployment requirements, rather than a newly stored component declaration hash.
