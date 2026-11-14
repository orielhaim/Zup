# Payload files

Each `[[files]]` declaration names a source mapping with a stable logical ID.
Matched files belong to an immutable application generation by default.

```toml
[[files]]
id = "application"
source = "**/*"
```

`source` is a pattern within the selected target's source directory. Relative
paths stay together within the generation, including adjacent libraries and
resources. Upgrades prepare a new generation before changing native entrypoints.

To place a source subtree below a different generation directory:

```toml
[[files]]
id = "assets"
source = "assets/**"
placement = { kind = "generation", directory = "resources" }
```

Use fixed placement when consumers require an exact physical location:

```toml
[[files]]
id = "system-resource"
source = "integration/shared.dat"
placement = { kind = "fixed_file", path = "${location.shared_data}/Acme/shared.dat" }
```

Fixed resources join the same installation plan and ownership record as versioned
payloads. Zup refuses replacement if the platform cannot meet the declared
replacement policy. A failed activation never changes placement automatically.

The default fixed-file replacement requires atomic exchange. Windows does not
provide an identity-conditional exchange, so applications that allow a brief
absence of the fixed path must declare `replacement = "quiescent"` inside their
placement. Zup then requires exclusive access to both file instances. A running
executable or another open consumer blocks replacement. The old instance stays
retained for recovery; Zup never overwrites its contents.

Mutable application data belongs outside the generation. Do not ship databases
or user configuration as immutable code resources.

Declare a directory that Zup should create and preserve:

```toml
[[install.data]]
id = "settings"
path = "${location.user_data}/${app.id}"
```

The application uses this directory directly. Zup records the directory's native
identity, rather than hashes of its mutable contents. Upgrades, repair, and
uninstall preserve it. Reinstalling the same installation can reattach its
recorded directory. An existing directory without authenticated ownership is
refused; matching its path or contents does not establish ownership.

Data paths must remain stable across versions. They cannot use `${payload}` or
`${app.version}`, overlap application files, or live inside Zup's `.zup` directory.
User installations create private user-owned directories. Machine installations
create private directories writable by SYSTEM and Administrators on Windows,
and root on Linux. To let static service accounts write a machine data root, declare its consumers:

```toml
[[install.data]]
id = "database"
path = "${location.shared_data}/${app.id}"
access = { kind = "services", services = ["worker"] }
```

Windows grants the exact service SID. Linux grants the resolved static UID with
access and default ACLs. Zup retains ownership of the root. New children inherit
access; existing mutable descendants are never traversed or rewritten. Writer
identities and placement cannot change for an existing data resource. External
permission changes require recovery instead of automatic permission repair.

For systemd-managed state, including DynamicUser services, use native placement:

```toml
[[install.data]]
id = "database"
service = "worker"
```

Systemd provisions `StateDirectory=` when the service starts and supplies its
path in `STATE_DIRECTORY`. A stopped service can own this native namespace
before a directory exists. Windows and OpenRC use an installer-owned native
state path with the declared service as its writer. `zup inspect` reports the
resolved path. Use explicit `path` placement when the application requires a
particular data location. DynamicUser services require native service placement.

Pass a native data path to the service as a complete argument:

```toml
[[services]]
id = "worker"
# Other service fields omitted.
arguments = ["--database", "${data.database}"]
```

The installer resolves the reference in the native registration. The application
receives an ordinary path and does not call Zup. References must name data that
grants this service access. Use a separate argument rather than embedding the
reference in text such as `--database=${data.database}`.

Data preparation, native registration, access verification, and service restore
are separate steps in the same deployment plan. A service starts only after its
data policy has been verified.

The default compatibility contract is stable across releases. For versioned data
formats, declare which schema the application writes and which older schemas it
can read:

```toml
[[install.data]]
id = "database"
path = "${location.user_data}/${app.id}"
compatibility = { kind = "schema", writes = 2, reads = [1] }
```

The written schema is always readable; `reads` adds other accepted schemas.
Zup refuses a deployment that cannot read the recorded data format. It does not
migrate databases. If new code may have written a format the old code cannot
read, failed activation requires recovery instead of automatic executable
rollback. Uninstall preserves data; it does not recursively purge these roots.

## Optional files

A pattern that matches nothing is an error by default. Declare an intentionally
optional source explicitly:

```toml
[[files]]
id = "extras"
source = "extras/**"
placement = { kind = "generation", directory = "extras" }
allow_empty = true
```

## Retained deployments and cleanup

Use `inspect` to list the active revision and retained rollback revisions.
`rollback --revision N` selects a retained deployment; `cleanup --keep N` retires
all but the most recent N previous revisions. Cleanup keeps one previous revision
by default. Retired revisions are no longer rollback targets.

Cleanup preserves the active revision and persistent data. It collects fixed
backup instances only when no active or retained snapshot references their native
identity. A modified or substituted backup is preserved and requires recovery.

Generation reader lifetimes are untracked by default, so cleanup retains those
physical generations. Applications that satisfy the following stricter contract
can opt into native image fencing:

```toml
[install]
generation_readers = "native_executables"
```

Every consumer must remain a native executable whose main image is in that
generation for its entire use of generation resources. This excludes scripts,
interpreter or loader-direct invocation, external applications loading its
resources, descendants that execute outside the generation and continue reading
it, independent late-use handles or mappings, and runtime writes to sealed files.
Binary inspection cannot prove this application contract.

On supported local Linux filesystems, executable writer handles exclude active
and new direct native execution. On NTFS, exclusive native handles exclude open
consumers and executable images. Zup retains a busy generation and retries it on
a later cleanup. Unsupported filesystems are deferred. No launcher, runtime lease,
or background cleanup process is required.

Interrupted deletion remains pending recovery. Process-crash tests cover prepared
collection, partial deletion, and temporary Linux image permissions. They do not
establish power-loss durability. Windows user-scope namespace durability does not
have the same documented directory `fsync` contract as Linux.

`inspect` reports recorded logical payload sizes, including retained collection
history. These are not measurements of allocated disk space. Mutable data,
filesystem allocation overhead, and interrupted preparation require additional
space.

## Compatibility diagnostics

`zup check`, `zup plan`, and `zup build` inspect PE and ELF payloads and report
declared native library dependencies and target conflicts. Inspection is advisory.
A dependency may come from the operating system, a prerequisite, or another
payload resource. Delayed loading and runtime-generated paths can introduce
additional dependencies.

Static inspection cannot prove that an application tolerates relocation, avoids
writing beside its executable, or has no external consumers of a physical path.
Keep ordinary application-relative resources together. Declare fixed placement
for physical-path requirements and persistent directories for mutable state.
Binaries larger than 64 MiB are not statically inspected; executable declarations
receive a diagnostic for that limit. These diagnostics never select a different
placement strategy.
