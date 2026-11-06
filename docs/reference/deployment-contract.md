# Deployment contract

All frontends and plugins use the same pipeline:

`BuildPlan → DesiredState → ExecutionPlan → TransactionPlan`

| Stage             | Responsibility                                                                                       |
| ----------------- | ---------------------------------------------------------------------------------------------------- |
| `BuildPlan`       | Compiled declarations, payload identity, and target capabilities.                                    |
| `DesiredState`    | Selected components, resolved paths, and typed resources, including plugin output.                   |
| `ExecutionPlan`   | Changes against observations and committed ownership, bound to the observed installation generation. |
| `TransactionPlan` | Operations, preconditions, dependencies, staging, and compensation order.                            |

## Ownership and authority

Resource keys, paths, content, executable intent, and privileges must agree across stages. Windows path comparisons use Windows semantics on every build host. Unknown resources are not absent resources.

Matching an existing resource does not confer ownership. Only committed receipts establish ownership. Uninstall uses that ledger without rerunning plugins or resolving current declarations. Modified and foreign resources are preserved; forced repair requires explicit `RepairOwned` intent.

Execution rechecks ownership, generation, and live preconditions under the installation lock. Privileged workers verify authenticated content and derive authorized effects themselves. User-writable state cannot authorize machine resources.

## Execution and recovery

Content is staged and verified before durable mutation intent. Affected services stop before their files change; startup and readiness verify before commit. Compensation stops the new runtime, restores configuration and files, then restores the previous runtime when safe.

Effects have stable operation IDs, durable intent, and receipts. Recovery re-observes native state before retrying uncertain effects. Ambiguity requires intervention. A committed transaction can still need ledger publication or cleanup; it must reconcile rather than replay installation. External process side effects are not atomic or reversible.

## Preview and automation

Authoring previews describe desired state without inspecting the machine and report `machine_state_observed: false`. Runtime previews use native observations; execution still rejects stale observations.

Progress carries typed phases and outcomes through workers and frontends. Presentation labels never determine engine behavior.
