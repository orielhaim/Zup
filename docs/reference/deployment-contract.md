# Deployment contract

Zup owns deployment. Applications, native services, shell registrations, and CLI
entrypoints run without a Zup process or library.

Compiled package requirements and plugin output become a resolved desired state.
Lifecycle planning compares that state with native observations and recorded
ownership. One validated `DeploymentPlan` graph authorizes preparation, activation,
publication, compensation, and post-commit cleanup. Every frontend and privileged
worker uses that graph.

## Placement and identity

Application payloads use immutable generations by default. Their content identity
includes verified bytes and execution-relevant metadata, independently of the
human-readable version. A physical materialization has its own identity. Repair
creates a new materialization instead of altering a committed generation.

Fixed files have explicit physical destinations and replacement policies.
Persistent directories have separate ownership and data compatibility contracts.
Native registrations refer to exact generation paths where relocation permits it.
A failed operation never changes a resource's placement policy.

Logical resource identities establish ownership. Paths locate resources; matching
paths or bytes do not establish ownership. Execution rechecks protected provenance,
native object identities, permissions, and content before changing owned files.
User-writable intent cannot authorize machine resources.

## Activation and publication

Generation acquisition and verification precede effects that expose its content.
Dependencies order affected services, persistent data access, fixed-file changes,
native registrations, and runtime verification. Unchanged services do not restart
because unrelated content changed.

The protected `InstallationRecord` publication and readback is the commit point.
Its latest commit identity is separate from the active snapshot's original
transaction and activation revision. Metadata maintenance preserves that snapshot.
A missing journal acknowledgment is reconciled against the published record.
Uncertain publication cannot trigger blind compensation.

## Recovery and cleanup

Operations persist intent before native mutations and receipts after observation.
Recovery reobserves uncertain effects. It refuses ambiguous ownership or runtime
state. Executable rollback does not reverse arbitrary process side effects or
incompatible database writes.

Cleanup retires rollback roots in the same authoritative record publication.
Irreversible deletion runs afterward. A cleanup failure leaves the deployment
committed and the cleanup recoverable; it does not roll back the active version.
Busy resources and generations without a sufficient reader contract are retained
with an explicit deferred result.

Materialization leaves a verified tree in private preparation storage. Sealing
first persists publication intent, then publishes access and the final directory
name. An interrupted access transition can resume only against the recorded
native identities and the exact private/public access profiles. Activation and
native registrations wait for the sealed tree. Windows file ACLs have no
inheritance flags; directory ACLs explicitly control inheritance.

Generation sealing records the native identity of every file and directory.
Verification, activation, and collection compare those identities with the protected
seal. Identical bytes in a replacement object do not preserve ownership. Collection
records its deletion census before changing permissions or removing entries. Native fences exclude declared consumers before
deletion. Recovery accepts only recorded remaining members and rejects substituted,
additional, or redirected entries. Persistent data is outside this collection tree.

## Preview and automation

Authoring previews describe requirements without claiming machine observations.
Runtime previews show actual typed effects and their authorization requirements.
Privileged workers independently derive their graph from authenticated intent and
current protected state before execution. The client reviews the worker's prepared
plan before it creates a transaction.

Progress reports staging, verification, activation, recovery, and cleanup. Cleanup
reports whether it removed content or retained it because a safety condition could
not be established.

Observed previews report separate logical byte counts for new generations, fixed
staging, verified reuse, retained fixed recovery files, and cleanup candidates.
Additional payload space is the sum of new generations and fixed staging. Existing
recovery files are retained rather than copied again. Cleanup candidates are not
credited against preparation space because collection may be deferred. These
counts exclude compressed downloads, caches, filesystem allocation overhead, and
mutable application data; they are not a free-space guarantee.

Explicit recovery reports successful completion for both a committed transaction
and a transaction whose compensation completed. A deployment attempt that had to
roll back still reports failure. The structured result distinguishes these cases.

## Native entrypoint paths

`${payload}` resolves to the exact selected generation directory when Zup builds
native registrations. Use it for launcher targets, service binaries, and PATH
entries. It is not a mutable `current` link. A running process keeps its original
application directory when a later deployment changes registrations. Newly opened
shells observe updated PATH registration; existing shell environments may retain
an older generation path. Retention and the declared reader contract govern when
that generation can be collected.

## OpenRC admission and process scope

OpenRC runlevel membership controls automatic startup. It does not block manual
starts. Zup stores the separate admission decision in
`/etc/zup/openrc/<native-name>`, in root-owned private directories and a private
regular file. The canonical service's `start_pre()` reads that exact file.
Only `allowed` permits admission. Missing or malformed evidence blocks native
startup and cannot authorize Zup to declare the service safely inhibited.

Manual and automatic services permit native starts. Automatic services also
belong to the default runlevel. Disabled services have inhibited admission.
Runlevel-specific configuration cannot replace the admission decision. Zup owns
the launch configuration, so service configuration files may contain only blank
lines and comments. Executable overrides, symbolic links, unsafe permissions and
foreign runlevel entries are configuration conflicts. Zup does not adopt or
rewrite those files.

Before stopping a service, Zup durably inhibits admission, removes automatic
startup, and drains OpenRC's native request lock. The lock covers starts admitted
before inhibition. Later starts fail in the canonical hook. Inhibition alone does
not stop an existing supervisor or its respawns. Shutdown retains pidfd identity
checks and requires the authenticated cgroup v2 scope to become empty before
replacement. Recovery settles interrupted native requests before observing their
terminal state. Restoring admission follows runlevel publication; uncertain
activation is inhibited and drained again before compensation.

`scripts/verify-stock-openrc.py` runs the native policy, override, concurrent-start
and process-crash regressions in a disposable Alpine VM. It also reboots the guest
to verify automatic startup and persistent inhibition without a Zup process.
The verification host needs `musl-gcc` for a test-only fixture that pauses OpenRC
while it holds its native request lock. Installed services need no extra helper.

## Windows service process scope

SCM reports the service's main process. It does not establish that child processes
have exited. Managed Windows services therefore declare
`execution = { type = "windows_job", account = "local_service" }` and receive their scope name
as a whole argument such as `"${job.worker}"` for service `worker`.
The job name is an installation-specific locator, never an ownership credential.

The service creates a new native Job Object before starting workers or reporting
Running. An existing object is an error. It assigns itself, enables
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, and permits neither breakaway flag.
Before doing application work, it protects the DACL and closes its initial
all-access handle. Final permissions grant SYSTEM, Administrators, and the exact
service SID only query, terminate, READ_CONTROL, and SYNCHRONIZE. An OWNER_RIGHTS
entry grants only READ_CONTROL. The owner is the configured service account.
Handles must not be inherited or exported. Workers must use normal inherited job
membership; brokered or deliberately escaped workers violate this contract.

Zup independently checks those permissions and limits, the exact SCM process's
membership and creation time, and its registered generation path. During an
approved transition it retains a non-inheritable query handle, closes native
startup admission, and requires the main process and all job members to exit
before replacing protected inputs. An empty old scope is released before the
next service start. No Zup process is needed after installation.

A missing job name after an interrupted stop is insufficient recovery evidence.
Recovery uses a protected verified-empty receipt, the retained original scope,
or a later native boot sequence together with a changed SCM identity and a
closed, stopped registration. Unsupported process telemetry, changed permissions,
remaining workers, or ambiguous identity require recovery instead of replacement.
The telemetry structure is documented by Microsoft, but its native acquisition
route is capability-tested and can be unavailable. The boot-environment GUID is
not used as a boot-session identity.

See Microsoft's [Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects),
[job access rights](https://learn.microsoft.com/en-us/windows/win32/procthread/job-object-security-and-access-rights),
and [process telemetry structure](https://learn.microsoft.com/en-us/windows/win32/devnotes/process_telemetry_id_information_type).

## Windows retained recovery source

Windows deployment has one durable source boundary. After the worker independently
validates a deployment and records its authenticated release identity, it copies
all required fixed-file, generation and maintenance content into the protected
transaction's `recovery` directory. Release resolution caches the exact authenticated
release, catalog and manifest documents for independent worker verification.
Previously retained fixed instances are opened
through their ownership receipts before copying. Generated files use the same
content inventory as other payloads. Each copy is streamed, size and digest checked,
flushed, published and reread before any deployment effect starts. The final source
context binds the transaction ID, plan fingerprint, authenticated release, generated
payload identity and exact content inventory. Machine content uses SYSTEM-owned,
Administrators-only directories and handle-relative native publication. User content
uses owner-private directory capabilities.

A recovery caller identifies the transaction inside its app, scope and target
boundary. The worker authenticates the parent process and carrier executable, opens
the machine-selected state root, acquires the installation lock, and independently
loads the journal and source context. Caller version, release, cache, payload and
overlay paths cannot select recovery content. The CLI and GUI use this same path;
Machine operations use the worker even when the caller is already elevated.
Recovery needs neither the original installer nor the acquired cache or network.
Missing or changed required content leaves the transaction pending with a diagnostic;
it never falls back to a package or another release. Untouched preparations can be
retired without payload bytes.

Carrier app/scope/target authorization remains mandatory, but carrier app version
is independent of transaction version. A previously installed v1 maintenance carrier
built with this contract can recover a v2 transaction only while both support the
same journal, plan, protocol and Windows execution format. Journal schema 2 marks
adoption of this contract, so workers predating the source-format gate refuse
these transactions. This engine refuses schema 1 journals; they require a compatible
trusted carrier for settlement. Execution format 1 is
explicit in the protected source context. Semantic execution changes must bump it,
even when existing JSON would still parse. Unknown formats are refused before
reconciliation. This does not retrofit recovery support into older shipped engines.
No retained arbitrary executable is launched and no resident service is required.

Recovery retains existing ownership and publication rules. It reobserves uncertain
effects through the transaction engine; an unreceipted fixed staging object remains
ambiguous and is preserved rather than adopted from matching bytes. Incompatible
persistent data or an irreversible external effect is not permission to roll back.
Requested or ambiguous publication must be reconciled by protected identity and
readback, without speculative rollback. Publication reconciliation does not prove
fresh service runtime state after reboot; that separate verification limitation
remains. A pending recovery is distinct from a committed deployment with remaining
reconciliation work.

Source content stays rooted until rollback settles, or commit, post-commit work and
ownership reconciliation all finish. Collection removes only declared members
through validated handles and verifies the empty directory's object identity before
removing it. Collection also checks the execution format and removes the context
after its blobs, so an older carrier cannot collect an incompatible future source.
Unknown siblings defer collection. Subsequent maintenance retries
settled-source collection before history cleanup. Input overlays may be removed once
the protected snapshot is complete. Process interruption tests do not establish
power-loss durability; native Windows ARM64 requires separate qualification.
