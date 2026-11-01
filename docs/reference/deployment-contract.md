# Deployment contract

Zup's deployment semantics live in `zup-plan`. Windows and Linux resolve native locations, inspect OS resources, and compile native effects against this contract. They do not define separate deployment models.

## Planning stages

1. **Compile the manifest and content.** `BuildPlan` describes available content, components, conditions, and plugin bindings. It is independent of the machine where the installer will run.
2. **Select desired state.** `plan` evaluates a concrete scope, components, conditions, and plugins. Its `PlannedInstallation` contains `DesiredState<Template>` and generated content. An active plugin requires an executor; omitting one fails planning.
3. **Resolve native locations.** Platform resolution produces `DesiredState<TargetPath, TargetPath>`. The same resource definitions now contain concrete paths, and launchers have concrete destinations. Linux desktop integration becomes generated files in this graph. Resource counts, sizes, and authorization requirements are computed from the graph rather than cached separately.
4. **Observe the host.** `HostSnapshot` reports facts for the desired resources. The committed `InstallLedger` supplies ownership and a typed transaction generation. Observations are joined by resource identity, independently of collection order.
5. **Plan a transition.** `plan_lifecycle` applies install, upgrade, modify, repair, or uninstall policy. Its `ExecutionPlan` contains typed changes, preconditions, conflicts, and owned retirements. A deployment binding fingerprints the resolved desired state and records the observed installation generation.
6. **Compile and execute effects.** Native lowering verifies the binding and the resource content and privileges it is about to compile. `TransactionPlan` adds staging, dependencies, durable operation identifiers, and rollback ordering. Execution repeats ownership, generation, and mutation precondition checks under the installation lock. The privileged Windows worker derives the complete transaction again from its authenticated installer and deployment intent, rejecting stale or altered effects. For acquired content, it re-verifies the runtime handoff, release documents, and embedded plan before reading the verified cache. Generated payload overlays use the same implementation for embedded and acquired content. A change of user context that changes resolved locations also invalidates authorization.

Source templates, host observations, native effects, and durable journal operations remain distinct because they have different responsibilities and trust boundaries. File decisions and preconditions are shared directly between transition planning and transaction compilation.

## Invariants

- A resource key agrees with its declaration. Maintenance resources also identify their application and version.
- Selected components and resource identities are unique. Resolved file destinations cannot overlap each other, launchers, or the installation directory. Windows comparisons and generated resource keys use Windows path semantics on every build host. Path equality, ordering, and hashing agree while preserving display spelling.
- Malformed observed resources remain distinct from absent resources. An unreadable service cannot authorize creation over an existing unit.
- Every concrete path belongs to the deployment's target. Capability checks apply to selected desired resources, including plugin output.
- Installation scope is a concrete selection. Resource privileges remain explicit and must match the ownership trust domain. System services require machine scope: a user-writable ledger cannot grant authority over system resources.
- Planned file content, executable intent, and privilege must match the bound desired state. Changing the desired content after observation invalidates the binding.
- The transaction carries the generation that was observed, rather than accepting a newer generation during lowering. An intervening installation change invalidates execution.
- Uninstall derives removals from committed ownership on both platforms. It does not resolve current source declarations or execute plugins.
- Forced repairs are explicit `RepairOwned` changes. Native lowering cannot independently turn drift into permission to overwrite a resource.
- A matching pre-existing file does not become installer-owned merely because it appears in desired state. Ownership comes from committed mutation receipts.
- Retirement and cleanup use committed ownership. Modified or foreign resources are preserved; interrupted transactions must settle before reconciliation and cleanup proceed.

## Preview and automation

Developer planning and preview resolve desired state using build locations. They do **not** inspect the current machine. Their output identifies itself as desired state or simulation, and automation reports `machine_state_observed: false`.

Native GUI and terminal planning use runtime preparation, host observation, and transaction compilation. Their previews describe changes against observed machine state. A preview is an explanation, not permission to reuse stale observations: execution prepares and validates its own transaction.

Progress events carry an explicit `OperationPhase` through the engine, worker protocol, console, and GUI. Labels are presentation text; they are not parsed to determine phases or outcomes. Completion and failure remain typed, including successful operations that require a reboot. Linux authorization uses pkexec’s documented exit statuses rather than parsing localized stderr.

## Recovery

The journal records operation intent and receipts around mutation. Recovery reconciles interrupted operations before retrying or rolling back. Pending publication follows observed generation edges, never UUID timestamp order; ambiguous histories fail closed. Completed reconciliation is idempotent and cannot republish historical ownership. A committed transaction may require further reconciliation if ledger publication or cleanup fails; presentation must preserve that outcome rather than report either a clean failure or a fully settled success.

Native backends retain OS-specific operations such as registry ownership checks, systemd lifecycle control, reparse-point protections, and authenticated privilege elevation. Shared contract validation supplements these checks; it does not replace them.
