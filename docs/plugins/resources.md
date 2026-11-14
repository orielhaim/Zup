# Plugin resources

A plugin returns a `Plan`; Zup merges its resources into the installation plan:

```rust
use zup_sdk::plugin::prelude::*;

let plan = Plan::new()
    .generated_file(GeneratedFile::text("configuration", "${install}/app.ini", contents)
        .replacement(FixedReplacement::Quiescent))
    .launcher(Launcher::menu("main", "Acme", "${install}/acme.exe"))
    .path_entry(PathEntry::new("cli", "${install}/bin"));

plan.validate()?;
Ok(plan)
```

`validate()` checks output limits locally; the host always validates the result.
A generated file's `id` must be stable and unique within its plugin. IDs contain
1 to 128 ASCII letters, digits, dots, underscores, or hyphens. Zup scopes ownership
to the plugin and ID, independently of the destination path. Two plugins can use
the same ID; their destinations must still be distinct.

| Method             | Constructor                                                                   |
| ------------------ | ----------------------------------------------------------------------------- |
| `generated_file`   | `GeneratedFile::new(id, dest, bytes)` or `GeneratedFile::text(id, dest, str)` |
| `launcher`         | `Launcher::menu(id, name, target)` or `Launcher::desktop(id, name, target)`   |
| `path_entry`       | `PathEntry::new(id, dir)`                                                     |
| `service`          | `Service::new(id, binary, execution)`                                         |
| `protocol`         | `Protocol::new(scheme, exec)`                                                 |
| `file_association` | `FileAssociation::new(id, ext, exec)`                                         |

## Launchers

```rust
Launcher::desktop("main", "Acme", "${install}/acme.exe")
    .with_arguments(["--background"])
    .with_working_directory("${install}")
```

## Services

```rust
Service::new("acme-agent", "${install}/agent.exe",
    ServiceExecution::WindowsJob(WindowsServiceAccount::LocalService))
    .with_display_name("Acme Agent")
    .with_start(ServiceStart::Automatic)
    .with_readiness(ServiceReadiness::Manager)
    .with_lifecycle(ServiceLifecycle {
        install: InitialServiceState::Running,
        change: ChangedServiceState::Preserve,
        timeouts: Some(ServiceTimeouts::Windows(WindowsTimeouts {
            start_observation_seconds: 60,
            stop_observation_seconds: 30,
        })),
    })
```

The `id` is what uninstall refers to and what a later install matches on, so it
must survive a version bump. Zup derives the native service name from the application and logical service ID. Use `.with_native_name(...)` only when external software requires a particular native name. Windows Job services must implement the native process-scope contract described in [services](/guide/services).

`ServiceStart` is `Automatic`, `Manual` or `Disabled`.

## Protocols and file associations

```rust
Protocol::new("acme", "${install}/acme.exe").with_arguments(["--open"])
FileAssociation::new("Acme.Document", ".acme", "${install}/acme.exe")
    .with_description("Acme document")
```

The association's id is the identity of the registration on the machine, so it
must be stable across versions. On Windows this is the ProgID the shell matches against.

## Ownership

Return only resources the application owns. Plugin resources follow the manifest lifecycle; conflicting declarations are refused.
