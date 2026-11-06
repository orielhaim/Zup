# Plugin resources

A plugin returns a `Plan`; Zup merges its resources into the installation plan:

```rust
use zup_sdk::plugin::prelude::*;

let plan = Plan::new()
    .generated_file(GeneratedFile::text("${install}/app.ini", contents))
    .launcher(Launcher::menu("Acme", "${install}/acme.exe"))
    .path_entry(Path::new("${install}/bin"));

plan.validate()?;
Ok(plan)
```

`validate()` checks output limits locally; the host always validates the result.

| Method             | Constructor                                                           |
| ------------------ | --------------------------------------------------------------------- |
| `generated_file`   | `GeneratedFile::new(dest, bytes)` or `GeneratedFile::text(dest, str)` |
| `launcher`         | `Launcher::menu(name, target)` or `Launcher::desktop(name, target)`   |
| `path_entry`       | `Path::new(dir)`                                                      |
| `service`          | `Service::new(id, name, binary, execution)`                           |
| `protocol`         | `Protocol::new(scheme, exec)`                                         |
| `file_association` | `FileAssociation::new(id, ext, exec)`                                 |

## Launchers

```rust
Launcher::desktop("Acme", "${install}/acme.exe")
    .with_arguments(["--background"])
    .with_working_directory("${install}")
```

## Services

```rust
Service::new("acme-agent", "AcmeAgent", "${install}/agent.exe",
    ServiceExecution::Windows(WindowsServiceAccount::LocalService))
    .with_display_name("Acme Agent")
    .with_start(ServiceStart::Automatic)
    .with_readiness(ServiceReadiness::Manager)
    .with_lifecycle(ServiceLifecycle {
        install: InitialServiceState::Running,
        change: ChangedServiceState::Preserve,
        start_timeout_seconds: 60,
        stop_timeout_seconds: 30,
    })
```

The `id` is what uninstall refers to and what a later install matches on, so it
must survive a version bump. `ServiceStart` is `Automatic`, `Manual` or `Disabled`.

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
