# Services

Zup manages owned Windows SCM, systemd, and OpenRC services through its deployment transaction.

```toml
[[services]]
id = "worker"
name = "Worker"
binary = "${install}/worker"
execution = { type = "systemd_dynamic_user" }
requires_files = ["${install}/worker.conf"]
start = "automatic"
lifecycle = { install = "running", change = "preserve" }
```

## Policies

- `start`: `automatic`, `manual`, or `disabled` controls boot startup only.
- `lifecycle`: defaults to `install = "stopped"`, `change = "preserve"`. Affected running services stop before mutation and resume afterward. Stopped services stay stopped. Unchanged repair avoids restarts. `change = "stopped"` leaves affected services stopped.
- `readiness`: `manager` verifies native running state. systemd uses `Type=exec`; `notify` instead waits for the main process to send `READY=1`. SCM and OpenRC reject `notify`. Manager readiness does not establish application health.
- Start and stop deadlines default to 90 seconds; configure `lifecycle.start_timeout_seconds` and `stop_timeout_seconds` between 1 and 3600.

## Dependencies

`requires_files` identifies managed configuration, libraries, assets, and other executables. `requires_components` expands selected components into their managed files. A required component must be selected. Changes stop running consumers before replacing their resources.

`requires_services = ["database"]` references another service in the installation. Cycles, missing services, cross-manager references, and unavailable prerequisites are refused. Dependents stop first; prerequisites start first. Dependency declarations never authorize control of foreign services. Plugins can declare file and service references through the SDK.

## Execution identities

Choose the native identity explicitly:

```toml
# Windows: network_service and explicit local_system are also supported.
execution = { type = "windows", account = "local_service" }

# systemd: an existing account, or systemd_dynamic_user.
execution = { type = "systemd_user", user = "worker", group = "worker" }

# OpenRC: an existing account; machine scope only.
execution = { type = "openrc_user", user = "worker", group = "worker" }
```

Windows registrations use a service SID. Password accounts are unsupported. Generated Linux definitions set `NoNewPrivileges`; OpenRC verifies actual process credentials. Machine-service payloads must be protected from untrusted writes. Applications must provision suitable writable storage.

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

systemd also accepts `condition = "on_abnormal"`. Each explicit deployment start resets its native start-limit budget; automatic retries remain bounded. OpenRC supervision requires a foreground process.

SCM recovery supports crash-triggered restarts only. Zup disables startup while replacing a restart-enabled service. SCM cannot report or cancel queued recovery actions, so preserving Stopped while reenabling a previously executed service is refused. Retiring its restart policy has the same restriction. A service never started in the current boot can be reenabled safely. Commands, reboot actions, and unsupported manager-policy combinations are rejected.

## Recovery

Zup stages verified content and records durable intent before shutdown. Startup must verify before commit. Compensation stops the new runtime, restores registration and files, then restores the previous runtime when safe.

External modifications, concurrent jobs, unknown runtime state, or insufficient interruption evidence require recovery. Keep the reported journal and diagnostics. Process side effects are not reversible. See the [plugin SDK](/reference/plugin-api) for typed declarations; plugins cannot control services directly.
