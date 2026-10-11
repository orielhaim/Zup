# Plugin resources

A plugin returns a `Plan`; Zup merges its resources into the installation plan:

```rust
use zup_sdk::plugin::prelude::*;

let plan = Plan::new()
    .generated_file(GeneratedFile::text("${install}/app.ini", contents))
    .launcher(Launcher::menu("Acme", "${launcher}"))
    .path_entry(Path::new("${install}/bin"));

plan.validate()?;
Ok(plan)
```

`validate()` is optional. It refuses an oversized plan at the point of the
mistake; the host enforces the same limit regardless.

| Method | Constructor |
| --- | --- |
| `generated_file` | `GeneratedFile::new(dest, bytes)` or `GeneratedFile::text(dest, str)` |
| `launcher` | `Launcher::menu(name, target)` or `Launcher::desktop(name, target)` |
| `path_entry` | `Path::new(dir)` |
| `service` | `Service::new(id, name, binary)` |
| `protocol` | `Protocol::new(scheme, exec)` |
| `file_association` | `FileAssociation::new(id, ext, exec)` |

Each constructor supplies the defaults. A new resource kind gets a constructor
of its own when the WIT grows one.

## Launchers

```rust
Launcher::desktop("Acme", "${launcher}")
    .with_arguments(["--background"])
    .with_working_directory("${install}")
```

## Services

```rust
Service::new("acme-agent", "AcmeAgent", "${install}/agent.exe")
    .with_display_name("Acme Agent")
    .with_start(ServiceStart::Automatic)
```

The `id` is what uninstall refers to and what a later install matches on, so it
must survive a version bump. `ServiceStart` is `Automatic`, `Manual` or `Disabled`.

## Protocols and file associations

```rust
Protocol::new("acme", "${launcher}").with_arguments(["--open"])
FileAssociation::new("Acme.Document", ".acme", "${launcher}")
    .with_description("Acme document")
```

The association's id is the identity of the registration on the machine, so it
must be stable across versions. On Windows this is the ProgID the shell matches against.

## Ownership

Return only resources the application should own. Once accepted, plugin resources
follow the same lifecycle as manifest resources. A plan that contradicts the application's own manifest is refused rather than applied. Do not return a fixed resource from a plugin merely to avoid writing it in the manifest.