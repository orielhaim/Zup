# Services

Zup manages Windows SCM and systemd services. OpenRC, service dependencies, runtime failure policies, and managed writable service directories are not implemented.

```toml
[[services]]
id = "worker"
name = "Example Worker"
binary = "${install}/worker"
execution = { type = "systemd_dynamic_user" }
requires_files = ["${install}/worker.conf"]
start = "automatic"
lifecycle = { install = "running", change = "preserve" }
```

## Startup and maintenance

`start` controls boot startup: `automatic`, `manual`, or `disabled`. It does not start the service during installation.

Lifecycle defaults are `install = "stopped"` and `change = "preserve"`. Set `install = "running"` to start a new service. Upgrades stop affected running services before replacing files and restore their previous running state afterward. Stopped services remain stopped; unchanged repair avoids restarts. `change = "stopped"` leaves an affected service stopped. Disabled services cannot request startup.

`requires_files` lists declared files consumed by the service, such as configuration or libraries. Changes and removals stop running consumers. Paths must be unique managed files with the same privilege as the service.

Start and stop deadlines default to 90 seconds and accept 1–3600 through `lifecycle.start_timeout_seconds` and `lifecycle.stop_timeout_seconds`. Uninstall currently uses 90 seconds.

## Identity and readiness

Windows requires an explicit identity:

```toml
execution = { type = "windows", account = "local_service" }
```

`network_service` and explicitly selected `local_system` are also supported. Registrations use a service SID. Password accounts and existing unmanaged failure actions or SCM dependencies are refused.

systemd accepts `systemd_dynamic_user` or an existing account:

```toml
execution = { type = "systemd_user", user = "worker", group = "worker" }
readiness = "notify"
```

Generated units set `NoNewPrivileges=yes`. Dynamic users inherit systemd's restrictions; applications must provide suitable writable storage themselves.

The default `readiness = "manager"` verifies SCM running state or systemd `Type=exec`; it does not prove application health. systemd `notify` waits for the main process to send `READY=1`. Windows rejects notify readiness. Unit startup and shutdown timeouts are currently fixed at 90 seconds, independently of Zup's deadline.

## Failures and recovery

Only owned services may be changed. Zup stages content and records durable intent before stopping a service. Startup is verified before commit. Rollback stops the new runtime, restores files and registration, then restores the old runtime when safe.

Changed configuration, concurrent manager jobs, unknown state, or insufficient interruption evidence require recovery instead of replay. Preserve the reported journal and diagnostics. Arbitrary process side effects cannot be undone.

Plugins declare the same policies through the [SDK](/reference/plugin-api); they cannot control services directly.
