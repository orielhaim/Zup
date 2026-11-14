# Services

Zup manages owned Windows SCM, systemd, and OpenRC services through its deployment transaction.

```toml
[[services]]
id = "worker"
display_name = "Worker"
binary = "${payload}/worker"
execution = { type = "systemd_dynamic_user" }
requires_files = ["${payload}/worker.conf"]
start = "automatic"
lifecycle = { install = "running", change = "preserve" }
```

## Policies

Service IDs are logical names within an application. Zup generates native names
from the application ID and service ID, so different applications can both declare
`worker`. `display_name` changes the label shown by the service manager.
Set `native_name` only when another application or a hard-coded service contract
requires a particular name. A systemd override must include `.service`.
Use a whole argument such as `"${service.worker}"` when the executable needs its
resolved native name. Dependencies continue to reference logical IDs.

- `start`: `automatic` enables boot startup, `manual` allows explicit startup, and `disabled` refuses native activation. Installation-time startup is requested separately through `lifecycle.install`.
- Display-name updates and transitions between `automatic` and `manual` preserve execution when runtime inputs stay unchanged. systemd failure-policy and timeout updates also preserve execution. Identity, command, readiness, and service-dependency changes require replacement. OpenRC retry and supervision changes require replacement; SCM failure-policy changes quiesce the service to protect against queued recovery actions. The transaction checks the same native execution generation around every live update; external restarts fail verification.
- `lifecycle`: defaults to `install = "stopped"`, `change = "preserve"`. Affected running services stop before mutation and resume afterward. Stopped services stay stopped. Unchanged repair avoids restarts. `change = "stopped"` leaves affected services stopped.
- `readiness`: `manager` verifies native running state. systemd uses `Type=exec`; `notify` instead waits for the main process to send `READY=1`. SCM and OpenRC reject `notify`. Manager readiness does not establish application health. systemd commands disable environment expansion and escape specifiers so paths and arguments remain literal. Zup verifies the manager's loaded command and execution flags and refuses additional executable lifecycle hooks, even when the on-disk definition matches.
- `lifecycle.timeouts` selects an explicit manager policy. Its type must match `execution`. Each component is 1–3600 seconds; each complete observation budget must not exceed 3600 seconds.

| Timeout policy | Defaults                                                                                                   | Native execution and Zup observation                                                                                                                                                                                                                                                                                                                                                                                      |
| -------------- | ---------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `systemd`      | `start_timeout_seconds = 90`, `stop_grace_seconds = 90`, `abort_timeout_seconds = 5`, `settle_seconds = 5` | Generates `TimeoutStartSec`, `TimeoutStopSec`, and `TimeoutAbortSec`. Start observation includes the native timeout and settlement. Stop observation includes grace, the bounded abort phase when a system policy requests it, and settlement. Notification can extend native manager timeouts; Zup's observation budget remains fixed. Requires systemd 254 or later to verify effective restart and execution policies. |
| `openrc`       | `start_timeout_seconds = 90`, `stop_grace_seconds = 90`, `kill_wait_seconds = 5`, `settle_seconds = 5`     | Generates native `TERM/90/KILL/5` retry settings. Start timeout bounds Zup observation; it is not a native exec timeout. During deployment, Zup closes native admission, drains admitted requests, signals the exact native process through a pidfd, and verifies an empty cgroup before replacement. OpenRC has process readiness only.                                                                                  |
| `windows`      | `start_observation_seconds = 90`, `stop_observation_seconds = 90`                                          | Bounds Zup's observation. SCM owns service startup and shutdown; these settings cannot force termination. Checkpoints and wait hints never extend the absolute deadline.                                                                                                                                                                                                                                                  |

For example, `lifecycle = { timeouts = { type = "openrc", start_timeout_seconds = 20, stop_grace_seconds = 45, kill_wait_seconds = 5, settle_seconds = 5 } }` allows 45 seconds for graceful shutdown before OpenRC escalates, then bounds confirmation of termination.

The installation ledger saves the resolved policy. Quiesce and removal use the installed policy; restore uses the replacement policy. Compensation uses the policy for the runtime it controls. An OpenRC supervisor keeps the retry schedule captured when it started, so changing that schedule requires replacement of the runtime.

Affected services cannot activate during payload replacement, including services that were already stopped. The transaction persists a systemd mask, an OpenRC disabled start guard, or SCM Disabled startup and verifies shutdown before changing files. Systemd stops under the installed timeout policy before reloading its mask. Registration updates retain the inhibitor until all payload mutations verify. Compensation closes activation before restoring files and releases it only after restoring the previous registration. These inhibitors survive reboot.

Repairing a damaged systemd definition requires an explicitly authorized owned repair, a stopped runtime without pending jobs, unchanged runtime inputs, and a final native reobservation. A damaged definition cannot authorize executable replacement or service startup.

Systemd notification can extend native manager timeouts with `EXTEND_TIMEOUT_USEC`; it cannot extend Zup's readiness deadline. A late readiness notification never produces a successful start receipt. If a manager request or runtime cannot be proven settled, the transaction requires recovery and retains its protected before-images.

## Persistent state

Declare mutable state under `install.data`, separately from the service payload.
For a DynamicUser service, `service = "worker"` selects systemd StateDirectory
and the application reads its path from `STATE_DIRECTORY`. Static services can
use explicit paths with declared service writer grants. Access is provisioned
even when installation leaves the service stopped. See [Persistent data](payload.md)
for placement, ownership, and data compatibility.

## Dependencies

`requires_files` identifies managed configuration, libraries, assets, and other executables. `requires_components` expands selected components into their managed files. A required component must be selected. Changes stop running consumers before replacing their resources.

`requires_services = ["database"]` references another service in the installation. Cycles, missing services, cross-manager references, and unavailable prerequisites are refused. Dependents stop first; prerequisites start first. Dependency declarations never authorize control of foreign services. Plugins can declare file and service references through the SDK.

## Execution identities

Choose the native identity explicitly:

```toml
# Windows: network_service and explicit local_system are also supported.
execution = { type = "windows_job", account = "local_service" }

# systemd: an existing account, or systemd_dynamic_user.
execution = { type = "systemd_user", user = "worker", group = "worker" }

# OpenRC: an existing account; machine scope only.
execution = { type = "openrc_user", user = "worker", group = "worker" }
```

Windows registrations use a service SID. Password accounts are unsupported. Generated Linux definitions set `NoNewPrivileges`; OpenRC verifies actual process credentials. Machine-service payloads must be protected from untrusted writes. Applications must provision suitable writable storage.

`systemd_dynamic_user` assigns a distinct generated user and group name to each native unit. Zup verifies those effective names, resolves the allocation through systemd in both directions, and checks the running process credentials. A static account collision or an allocation that cannot be verified is refused. `DynamicUser=yes` alone does not establish a dynamic identity.

systemd control uses the local system bus and requires the system manager's filesystem root and mount, PID, and user namespaces. Caller-provided bus addresses do not change this authority. Controlling a host manager from a container or chroot with a different filesystem view is refused.

## Runtime failure policies

The default is no automatic restart. Native policies remain explicit because their limits have different semantics:

```toml
# systemd: bounds starts in a rate window, including explicit starts.
failure = { type = "systemd", condition = "on_failure", delay_seconds = 2, start_limit_burst = 3, start_limit_interval_seconds = 60 }

# SCM: two crash-triggered restarts, then no action; reset after 60 seconds.
failure = { type = "windows", delay_seconds = 2, restart_attempts = 2, reset_seconds = 60 }

# OpenRC supervise-daemon: bounds respawns in its native period.
failure = { type = "openrc", delay_seconds = 2, max_respawns = 3, respawn_period_seconds = 60 }
```

systemd also accepts `condition = "on_abnormal"`. Each explicit deployment start resets its native start-limit budget; automatic retries remain bounded. OpenRC uses stock `start-stop-daemon` when respawning is disabled. An explicit OpenRC failure policy selects stock `supervise-daemon` with bounded native respawns. Both policies require the application to remain in the foreground.

SCM recovery supports crash-triggered restarts only. Zup disables startup while replacing a restart-enabled service. SCM cannot report or cancel queued recovery actions, so releasing activation while preserving Stopped is refused for an existing registration with a recovery policy. Retiring that policy has the same restriction. Initial protected creation can release a new registration without starting it. A service exit code, including `ERROR_SERVICE_NEVER_STARTED`, does not prove execution history. Commands, reboot actions, and unsupported manager-policy combinations are rejected.

OpenRC service executables must be native ELF images matching the installation target. Shebang scripts are unsupported. The main process must remain in the foreground without daemonizing or replacing its image. Zup verifies the native pidfile, supervisor-child relationship where applicable, executable inode, process birth time, credentials, and protected cgroup identity. Shutdown uses a pidfd for the exact observed native process, then terminates remaining cgroup members after graceful shutdown. A missing completion receipt requires native reobservation.

OpenRC service sources are ordinary owned file effects. Activation is separate: Zup owns only the canonical `zup_disabled=yes` setting in the service's native `conf.d` file and its default-runlevel membership. Other overrides are refused. Closing this gate survives installer exit and reboot. Zup drains OpenRC's existing admission lock before shutdown; it does not rely on that transient lock as a permanent activation gate.

## Recovery

Zup stages verified content and records durable intent before shutdown. Startup must verify before commit. Compensation stops the new runtime, restores registration and files, then restores the previous runtime when safe.

An unfinished transaction blocks new operations on that installation. Run `recover --transaction <id> --scope machine` before retrying. Preparing an uninstall does not resume an interrupted upgrade or start its services.

External modifications, concurrent jobs, unknown runtime state, or insufficient interruption evidence require recovery. Keep the reported journal and diagnostics. Process side effects are not reversible. See the [plugin SDK](/reference/plugin-api) for typed declarations; plugins cannot control services directly.

Transaction plans and journals use schema 1. Journals retain the interrupted phase when native reconciliation requires recovery. A later recovery attempt rechecks authoritative evidence before continuing. A failed deployment continues compensation rather than resuming forward installation. Journals from earlier prototype formats are rejected; there is no migration path.

OpenRC requires cgroup v2 mounted at `/sys/fs/cgroup` during installation and service startup, kernel pidfds, and `cgroup.kill`. Configure the distribution's cgroups boot service before deploying automatic services. For Alpine, configure unified cgroups in `/etc/rc.conf` and add `cgroups` to the boot runlevel. Zup checks native scope controls and process membership; it does not mount a host filesystem during validation. Ordinary native service stops use OpenRC's cgroup cleanup after its retry schedule.

Stock OpenRC is supported. No patched supervisor, s6 dependency, Zup daemon, or Zup launch helper is required. Zup's service transaction remains responsible for its deployment-time gate, native process verification, and recovery. Native manager changes and ambiguous runtime evidence fail closed; reboot does not establish successful completion of an interrupted deployment.

After a manager restart or reboot, recovery can compensate an interrupted deployment when the exact owned registration remains disabled, native status is stopped, and the current native process scope is empty. It records fresh manager authority before compensation and preserves the original operation's historical identities. Recovery restores the prior deployment policy; it does not recreate the original process instance. An open gate, pending native work, changed registration, or uncertain process scope requires recovery without further payload mutation.

Service controls follow each service's executable, declared `requires_files`, and native definition. Related fixed payloads are placed before publishing that definition. The engine rechecks quiescence before each related fixed mutation, including when resuming a previously verified transaction. Changing unrelated payloads does not create a service restart requirement.
