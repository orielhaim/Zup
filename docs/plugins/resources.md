# Plugin resources

A plugin returns a `Plan`; Zup merges its resources into the ordinary
installation plan. `Plan` is a builder, because every plugin writes the same
shape - start empty, declare two or three things, return it:

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
mistake rather than after a round trip to a host, and the host enforces the same
limit regardless.

| Method | Constructor |
| --- | --- |
| `generated_file` | `GeneratedFile::new(dest, bytes)` or `GeneratedFile::text(dest, str)` |
| `launcher` | `Launcher::menu(name, target)` or `Launcher::desktop(name, target)` |
| `path_entry` | `Path::new(dir)` |
| `service` | `Service::new(id, name, binary)` |
| `protocol` | `Protocol::new(scheme, exec)` |
| `file_association` | `FileAssociation::for_extension(ext, exec)` |
| `resource` | a `ResourceItem` the SDK has no constructor for |

Each constructor supplies the defaults, so a plugin names the field it means
rather than the shape of the whole record.

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
should survive a version bump. The `name` is the internal service name.
`ServiceStart` is `Automatic`, `Manual` or `Disabled`.

## Protocols and file associations

```rust
Protocol::new("acme", "${launcher}").with_arguments(["--open"])
FileAssociation::for_extension("acme", "${launcher}").with_description("Acme document")
```

A file association's id is derived from the extension, because it has to be
stable across versions and unique on the machine, and the extension is the only
part of it an author chooses.

## Ownership

Return only resources the application should own. Once accepted, plugin resources
follow the same lifecycle as equivalent manifest resources: they are part of
planning, installation, repair, modification, upgrade and uninstall.

Everything a plugin declares is checked against the application's own manifest
before it is applied, so a plan that contradicts what the application declares is
refused rather than obeyed.

Do not return a fixed resource from a plugin merely to avoid writing it in the
manifest. Static declarations are easier to read and audit in `zup.toml`.

The Rust names above correspond one-to-one with the WIT records in the
[Plugin API reference](/reference/plugin-api).