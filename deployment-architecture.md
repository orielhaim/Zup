# Deployment architecture implementation contract

Status: implementation contract. Native lifecycle and recovery paths are implemented and under verification; `notes.txt` records remaining work and verified results. This document states the intended invariants, not a completed verification claim.

## Caller experience

Ordinary projects ship a payload tree and name a main executable. Shipped code defaults to generation placement. Desktop launchers, protocols and services use `${payload}` paths within the declared payload tree; Zup resolves them to exact generation paths and validates them against logical resources. Advanced declarations add fixed payloads, persistent directories and explicit alias compatibility. A Windows CLI alias is not synthesized from an unsafe mutable junction.

Fixed declarations carry a replacement contract. Atomic replacement permits no interval with an absent public file. Quiescent replacement permits that interval only while an authenticated runtime boundary establishes safe exclusive access. Declaring compatibility does not establish quiescence. Without the required native capability or runtime evidence, planning or execution refuses the replacement. Linux exchange support and Windows no-replace moves must not become silent alternatives to each other.

Keep this choice inside fixed placement through authored mappings, resolved payload metadata, bundle validation, desired resources, file effects and the authenticated graph. Generated native definitions and plugin files need an explicit policy too. Recovery uses the policy and strategy already bound into the journal.

The frontend submits an operation request and authenticated artifact selection. The authority resolves that request against authenticated content, current installation ownership and current native observations. It derives one DeploymentPlan. All frontends display that same plan, and the coordinator executes that exact validated graph.

## Domain shapes

- ApplicationId identifies the publisher's application.
- InstallationId is a UUID assigned on fresh installation and retained across operations.
- ResourceId is a logical declaration name scoped to InstallationId; it is not a destination path.
- DeploymentContractId is a domain-separated SHA-256 of canonical generation contents and execution-relevant metadata.
- MaterializationId identifies one physical verified copy of a ContentId. Repair can reconstruct another copy without modifying a committed generation in place.
- ActivationRevision is a checked, monotonic revision of the installation's selected deployment.
- TransactionId identifies an attempted transition, independently of activation and display version.
- Display version remains release information, never content identity or ownership proof.
- A fixed-file instance is identified by its origin transaction, logical resource and native filesystem identity. Moving that object in a later transaction does not transfer its origin.

DesiredDeployment has one map of ResourceId to ResourceRequirement. Use variants to prevent contradictory placements:

```
ResourceRequirement =
    Payload { source, placement: Generation { relative_path } | Fixed { destination } }
  | PersistentDirectory { destination, owner, uninstall_policy }
  | ConfigurationDefault { destination, default_content, update_policy }
  | Service { executable: PayloadRef, arguments, lifecycle, native_dependencies }
  | Entrypoint { target: PayloadRef, exposure }
  | Protocol { target: PayloadRef, arguments }
  | Association { target: PayloadRef, arguments }
  | SearchPath { directory: DirectoryRef }
```

PayloadRef names a logical resource plus a relative subpath when applicable. It never embeds the generation directory before DeploymentContractId calculation. Native registrations cannot be given file-placement flags. Persistent resources cannot be generation members. Privilege and authorized location are derived and validated at the authority boundary, not accepted as permissions from plugin output.

All owned files use `FileResourceId`, distinguishing authored payloads, plugin outputs, native integration files and maintenance artifacts. Destination paths are placement attributes. Launchers and PATH entries carry declared application or plugin IDs; display names are mutable presentation. Native desktop files derive their names from declared launcher IDs, so declaration order and display-name changes do not transfer ownership. Native roles select service, environment and cache policies; they do not grant placement permission.

Package metadata and blob streams retain the opened package file. Compressed regions stream through bounded readers with independent positions. Consumers still verify expected sizes and digests before publishing content; holding a source handle does not establish content immutability or deployment authority.

DeploymentContractId covers the target contract, sorted generation-relative names, file sizes and digests, executable metadata and symbolic execution requirements. Exclude source-machine paths, display version, installation UUID, physical materialization paths, activation revision and mutable data. Validation rejects duplicate/case-equivalent paths for the target, ancestor conflicts and references outside the selected resource graph.

## Single planning and execution graph

Keep artifact construction separate from deployment planning. Preserve three responsibilities: desired requirements, a freshly observed resource transition, and the authorized DeploymentPlan DAG. The observed transition supplies one immutable identity and activation context; platform lowering contributes native effects rather than copying the whole transition into another plan. Only the validated DAG is the durable executor contract. Authoring previews consume requirements and explicitly disclose when machine state was not observed.

The graph and its typed effects live in zup-plan; zup-transaction owns durable execution and recovery. zup-core owns shared identity, content and authoring contracts. Platform crates observe and execute typed platform effects, with no independent lifecycle planner. Build/package/preset/plugin workflows feed the shared requirements; they do not invent transaction effects.

Effects distinguish acquisition, content verification, materialization, sealing, persistent setup, fixed replacement, native observation/configuration, service coordination, activation verification, ownership publication, retirement and cleanup. Dependencies express each resource's requirements. Global prepare-before-visible-mutation is a safety boundary; ordering among fixed and native effects derives from their actual dependencies.

ReserveInstallation atomically binds the installation UUID to the application, scope, target and installation directory in protected state before staging or mutation. The reservation is authenticated against its originating protected journal. Concurrent claims use exclusive publication. Existing claims are reread with a durability barrier; missing established claims require recovery. Reservations survive compensation, and their origin journals remain retention roots.

A privileged authority re-derives effects from authenticated artifacts and current trusted ownership, and verifies the binding at execution. Serialized DAG validation is necessary but does not authenticate intent. Hold an installation lock and appropriate namespace/ownership authorities throughout the mutation boundary; handles and locks must not escape into children.

## Storage and repair

Use per-installation storage: private authority state and journals, readable protected payload materializations, and separate persistent paths. Machine payload access is policy-aware native authority, not owner-private PrivateDir permissions. Never use unverified content-cache presence as evidence.

Materialize with bounded copying into a new same-volume staging directory. Hash and verify through retained handles; persist contents and directory entries before publishing the sealed instance. CoW can optimize a verified copy where supported; ordinary isolated copies are the baseline. Cross-generation hardlinks are excluded.

A corrupt committed instance is never patched in place. Repair reconstructs a new instance with the same ContentId, verifies it and performs a normal activation transition. Old damaged instances remain pinned or quarantined until safe retirement. Fixed resources use observed content and ownership preconditions; repair cannot adopt foreign modifications.

Fixed ownership binds a logical resource to its originating transaction and native filesystem object. Content equality alone cannot establish continuity. Replacement and removal preserve that exact object in protected same-volume storage; compensation restores it by identity. Retained snapshots and unresolved journals keep the original storage reference alive even when a later transaction moved the object. Reconstructing a missing file creates a new instance, while force repair requires continuity with the owned object.

Native namespace operations retain validated parent handles and exclude foreign replacement. If preserving the old instance requires two moves, the plan must establish an activation boundary that permits the temporary absence of the fixed path. Otherwise it defers or refuses the operation. A backup link followed by unconditional replacement does not establish an exact-object exclusion guarantee.

## Activation and the commit point

An activation describes the selected materialization and the complete fixed/native resource state, separately from observed running service generations. A prepared materialization, configured service executable and running executable may differ during a recoverable transition.

Native registrations point at exact materialization paths. Alias exposure requires an authoritative application compatibility declaration and platform validation. The Windows junction experiment is a counterexample to a default alias guarantee. Fixed payloads remain fixed even if generation activation would be convenient.

The commit point is durable publication of one InstallationRecord containing ActivationRevision, selected materialization, owned-resource evidence, persistent policy, retained instances and committed TransactionId. Do not retain the current coordinator-commit/ledger-publication gap as two independent authoritative outcomes. The graph ends in PublishInstallation. The protected publisher first prepares the complete ownership and retained-state projection. The coordinator persists Requested with that complete record hash before writing. The publisher verifies or reconstructs the next record, publishes durably and reads back through its native authority. A missing final journal acknowledgment is recovered by validating and durably republishing the same record. A retry must match that original complete-record hash. Native authorization cross-checks installed ownership against its protected originating journal. Requested publication cannot enter compensation; an unresolved outcome remains recovery-required.

Before this point, required native mutations may be externally visible. Zup does not promise global atomicity. The journal durably records before/after requirements and compensation evidence before each effect. A crash requires native re-observation. Publishing an uncertain record requires a verified reread and durability barrier; missing completion receipts never imply non-execution.

Operation states distinguish planned, prepared, requested/uncertain, applied, verified, compensated and recovery-required. Compensation is typed per effect; persistent application side effects are not implicitly reversible. Unsupported or ambiguous service boundaries defer or refuse activation.

## Native integration caches

MIME and desktop application caches are shared derived state. `NativeCacheOperation` identifies the native generator, directory, and changed source resources in the deployment graph. Cache files have no installation ownership entries or byte-snapshot rollback.

Forward execution changes sources before regeneration. Compensation restores those sources, regenerates the cache, and then permits the old service to resume. The compiler derives these compensation dependencies from typed effects and validates the persisted rollback order. A pending forward cache effect still requires compensation if its sources changed.

The journal distinguishes a completed generator invocation from uncertain execution that requires compensation. Missing completion evidence cannot prove nonexecution. A failed compensation remains recovery-required until the generator succeeds against restored sources. There is no post-rollback cache sweep.

Linux user-scope execution holds an exclusive lease on the native directory inode through settlement. The lease coordinates Zup processes across installation state roots. External package managers do not honor that lease. The executor rejects links and foreign writable entries in the cache tree, opens a protected root-owned ELF generator from `/usr/bin`, and executes that held object with a sanitized environment. Only that generator inherits its cache lease, so it remains exclusive after installer termination. Installation locks remain close-on-exec.

Machine-scope cache execution currently refuses because it lacks a native authorization policy. Generator success proves completion of that invocation. It does not prove globally atomic cache updates, exclusion of later external writes, or power-loss durability.

## Retention and cleanup

Roots include committed and pending activations, all nonterminal recovery evidence, retained policy instances, native registration targets and conservatively observed runtime use. An absent main PID is insufficient. If native observation cannot establish safe reclamation, keep the instance and explain the storage cost. Root calculation alone does not prove runtime non-use.

Snapshot the intended cleanup set and revalidate ownership, file identity and roots immediately before removal. Preserve the only recoverable copy. No background daemon or hidden collector is introduced.

## Services and mutable state

SCM, systemd and stock OpenRC registrations use generation-pinned executable paths. Only affected services are coordinated; preserve prior running/stopped state according to policy. OpenRC uses native start-stop-daemon or supervise-daemon according to the declared respawn policy. Its canonical conf.d inhibition, native admission drain, pidfd process identity and retained cgroup identity form the deployment stop boundary. No patched supervisor or s6 dependency remains.

SCM ownership binds the protected installation UUID and logical service identity in an ACL-protected registry marker. Configuration equality does not prove ownership. Creation remains disabled until marker publication, hive flush, and readback succeed. The executor retains claimed SCM handles across transaction steps. These handles prevent deletion and same-name recreation from changing the native object during coordination. Recovery reacquires and validates authority after process exit; handles are never serialized or inherited by applications.

Removal releases a retained handle only while an independently validated deletion handle remains open. Successful `DeleteService` acknowledgment is insufficient: completion requires observed absence after closing that handle. Outstanding external handles or a running service leave deletion pending. Because Windows deletes the service registry subtree, undeclared values or subkeys block removal and preserve application data. A configuration failure after creation retains the disabled registration for recovery.

The marker protects against lower-privilege substitution. It is not an immutable SCM object identifier or protection against an equally privileged administrator. Runtime manager and process generations remain separate evidence.

Installed service ownership stores the current registration contract. Zup never adopts a foreign service, so uninstall has no foreign predecessor to restore. Transaction journals retain the before-state for authorized updates. Repair compensation refuses to restore an externally modified or unrecognized registration on every native manager. A failed repair retains its repaired state and requires recovery before the old runtime can resume.

Binary rollback activates retained verified content and restores validated native state. Data compatibility is a separate declared constraint. An irreversible data migration blocks incompatible rollback/downgrade. No arbitrary privileged shell hooks.

## Alternatives and synthesis

Generation-centric and resource-centric sketches were independently explored with Luna. Select one installation authority with a typed resource requirement map and a single effect graph. Keep the generation-centric distinction between content and activation; use resource variants to represent mixed placement without side engines.

Reject generic content-plus-placement products, which allow native or persistent resources to claim generation-file semantics. Reject ResourceId wrapping existing destination-keyed ResourceKey. Reject independent vectors duplicating placement categories. Reject GC based only on journal roots without runtime uncertainty. Reject adding generation metadata beside the old lowering chain as a final architecture.

## Content identity

GenerationDescriptor hashes a schema-1 preimage containing the domain tag, canonical target, sorted member count, and each generation-relative path, size, SHA-256 and executable flag. Variable fields are length-delimited. Physical paths, installation identity and native activation configuration do not enter this payload identity. The complete deployment authorization additionally covers fixed, persistent and native requirements. Payload identity alone never authorizes their effects.

TargetPath and the identity contracts live in core. The planner owns the effect graph, and transaction owns execution and recovery. Platform executors implement retained filesystem operations and native integration.

## Fixed-file instances

Fixed payloads have a logical resource identity, a physical file instance, and a required public destination. An instance is identified by its originating transaction and resource identity; the operating system's file identity and verified content are separate evidence. Stage files on the destination filesystem in a protected per-installation recovery namespace. A later replacement moves the original object into its stable instance slot, then publishes the staged object. Compensation restores that original object. Copying its bytes would create a different physical instance and invalidate the prior ownership binding.

Retained snapshots root fixed-file instance slots even when a later transaction moved those files there. Unchanged owned files reuse their instance. Matching bytes on a different or unowned object do not permit adoption, replacement or deletion. Force repair requires continuity of the owned native object; displaced external modifications remain quarantined. Restoring an absent owned file creates a new instance with authenticated content.

Windows replacement must use validated retained-parent authority and native identity checks. Native no-replace handle renames preserve object identity and refuse a foreign destination. Windows quiescent replacement holds exclusive read/write/delete file handles with no sharing on both native instances. Existing consumers and mapped executable images prevent that acquisition. It moves the original to a journal-derived same-volume retired name, then publishes the staged instance without replacement. A crash reconciles destination, stage and retired names against both authenticated native IDs; partial publication is compensated. Compensation reacquires both fences, preserves the displaced new instance, and restores the original native object. Atomic placement is refused rather than silently changed to quiescent placement. ReplaceFileW preserves the displaced original in its backup but has documented partially applied failure outcomes and no supported write-through flag. Neither API provides a global transaction. Linux atomic replacement requires exchange support and refuses filesystems that cannot provide it. Both platforms reobserve uncertain effects before continuing or compensating.

## Runtime requests

RuntimeOperation separates a deployment carrying its finalized plan and source intent from recovery naming an existing transaction. Bootstrap and release inputs belong only to deployment. Recovery loads the protected journal and binds its application, version, scope, target and payload provenance. A frontend cannot supply an alternate recovery plan. Both native backends use this contract; machine recovery uses the authenticated worker boundary.

Reconciliation authenticates the current installation first. A superseded transaction must be retained there with the matching plan hash. It cannot republish old ownership or execute stale cleanup against a newer activation.

## Native materialization protocol

One shared GenerationStorage state machine coordinates both platforms through GenerationFilesystem. The filesystem authority opens scope-appropriate protected metadata and originating journals, creates exclusive staging directories, validates native identities, writes bounded verified members, checks the exact tree, and publishes without replacement on the same volume. Native directory capabilities enforce user or machine ownership. Machine payload publication grants read and execute access separately from private journals and read-only installation records.

The protected preparation record precedes payload writes. Materialization advances from Preparing to Prepared while the tree remains private. Sealing persists Publishing before changing access or publishing the final directory name, then records Sealed after verification. Recovery reobserves staging and final native identities rather than inferring non-execution from a missing receipt. Unrecorded staging orphans are retained; an occupied unrecorded final path is refused. Compensation retains instances unless runtime non-use is established.

Windows machine payload ancestors must have trusted machine ownership and no applicable untrusted mutation grants. Unsupported ACL forms are refused. User and machine journals are separate authorities. File and namespace barriers are executed through their native capabilities. Process termination tests establish recovery at those tested boundaries, not power-loss durability.

InstallationRecord publication is the coordinator commit point. Native fixed-file identity evidence binds ownership, preconditions, displacement slots, and receipts. Collection requires the declared reader contract and a native exclusion boundary. Where no boundary prevents a new consumer from starting during reclamation, retain the instance.

## Canonical requirements and physical bindings

`PayloadTreeId` identifies only the verified immutable file tree.
`DeploymentContractId` identifies the canonical hybrid resource contract.
Neither identity is a physical materialization UUID or a display version.

Persist a typed, canonical deployment requirement map in each committed snapshot.
Its resource identities and execution requirements remain independent of the
materialization UUID. Keep payload-tree identity separate from the identity of
the complete hybrid deployment contract. Hash normalized requirements and
verified content, excluding physical generation paths and the computed identity.

Resolve symbolic payload, installation and native-location references through a
validated installation binding. Native configuration and file receipts record
the resulting physical state and ownership evidence. They do not replace the
canonical requirement contract.

Repair must compare the requested canonical contract with the authenticated
committed contract before selecting replacement bindings. An equal payload tree
alone does not authorize different native commands, arguments, placement,
privileges or persistent-data policy. A verified materialization binding change
may repin native entrypoints to the same logical payloads. Keep the exact prior
ownership and observed state as mutation preconditions and compensation evidence.
Never infer this authority from a generation-path prefix.

PATH declarations require stable semantic identities independent of their
resolved directory values. Generated Linux integration files derive from the
same canonical native requirements; their rendered bytes and native file
identities belong to the physical execution plan. Worker authorities recompute
these bindings from authenticated requirements and current state.

The symbolic `DesiredState` is bound by the platform resolver into a
`BoundDeployment` carrying its canonical `DeploymentRequirements`. The binding,
effect graph, protected journal and every retained snapshot preserve that same
contract. Removal preserves the last contract as history while removing active
ownership. Missing contracts are invalid; there is no adoption of older records.

Repair rebinding derives both paths from an anchored, typed payload reference
and the old and new descriptors. An executable must identify an exact declared
member; a directory must identify the generation root or a declared member's
parent. The prior native state must still match recorded ownership. A changed
contract requires Upgrade or Modify. This does not authorize overwriting
externally modified native state.

Windows PATH uses a stable application or plugin entry ID. Rebinding replaces
one observed owned segment under a shared scope lock and preserves unrelated
segments verbatim. The lock coordinates Zup writers; it does not serialize
arbitrary external registry writers. A missing insertion or rebinding receipt
with the desired segment present remains ambiguous rather than creating an
ownership claim from membership alone.

### Maintenance artifacts

Maintenance executables and preset assets are immutable deployment content with
an internal purpose.
A private-artifact generation uses the same materialization protocol and journal
as the application generation, under `.zup/maintenance/<materialization-id>` in
the installation store. Here private means internal to installation management;
machine-scope code remains readable and executable by users, with protected
system ownership and writes.

The contract is `GenerationPublication::Application { previous,
revision } | Maintenance`. Only the application variant selects an application
activation revision. Maintenance content is sealed before any native icon or
maintenance command references it; publication records its materialization in
the same installation snapshot. Generation sources use logical resource keys,
including explicit maintenance artifact identities, rather than destination
paths or display versions.

Repair reuses the authenticated maintenance generation when the application
contract is unchanged. It does not replace that executable merely because the
invoking installer was rebuilt. Missing or corrupt maintenance content requires
a source matching its recorded verified identity. Uninstall retires its native
references and ownership selection; conservative generation retention and cleanup
handle in-use maintenance content. There is no separate artifact transaction
engine or required cleanup process.

Modify reuses maintenance content only when the complete runtime, preset, asset
descriptor and installed preset metadata match the committed snapshot in the same
installation namespace. Display-version equality is insufficient. Explicit
materialization requests still prepare a separate physical instance.

User installations authorize the application directory through retained native
handles and protect `.zup` with a private ACL. Fixed payloads can occupy the same
application directory without imposing journal ACLs on that public namespace.
Maintenance runs directly and returns its transaction outcome synchronously;
there are no self-copy uninstall or terminal runners.

Online Windows releases carry a complete per-application plan in their native
runtime image. Acquisition retains and verifies that exact image; it never
recomposes a signed runtime. Maintenance preparation reads the selected release
image, rather than the image that initiated the update. Native handle pins deny
file mutation and ancestor renames throughout preparation and execution.

For privileged updates, the existing worker remains the executing program. The
selected image is input data. The worker authenticates its release against its
own embedded application identity, target, and TUF root, then independently
compiles its complete plan. The same selected bytes become the maintenance
generation. A user cache path is never selected as a new elevated program.
Changing the initial embedded trust root requires an explicit authority
transition; ordinary source selection refuses it.

The installation records the release document digest, graph and artifact
identities, repository locator, typed channel/version selection, and initial
trust anchor. Interrupted transaction recovery binds those facts to the
protected journal and exact source image. Recovery does not reinterpret a
moving channel or require a fresh online authorization of an already authorized
transaction. Future maintenance must use the recorded release source without
depending on a transient cache path.

## Release source identity and input authority

A release selection is either a channel or an exact release document digest paired with its display version. Release, catalog, and variant documents have immutable digest addresses; display-version paths are absent. The installation records its authenticated source identity. Repair and Modify reselect that exact source, while an explicit Update may follow the channel to different content at the same display version. Historical documents must remain authorized TUF targets, as retaining bytes alone does not establish trust.

An elevated worker executes its authorized installer image. A newly selected release image is pinned data, independently authenticated and replanned before effects. Caller-owned images, cache entries, overlays, and local repository files are opened under the authenticated parent token. Synchronous scopes restore the prior thread token before asynchronous work; returned streams use already-open handles. Protected installation state and TUF state retain machine authority. Source permissions do not grant deployment permissions.

The source boundary does not establish TUF datastore durability. Tough's current datastore uses ordinary path writes and per-instance synchronization; protected durable publication and cross-process repository serialization remain required work.

## Repository trust persistence contract

Application identity and the embedded initial trust-root digest identify the repository trust domain. Channel names, mirrors, and repository URLs select transport and targets; changing them must preserve observed metadata version floors. Independent repositories require independent roots.

The native repository store must hold an exclusive cross-process lease throughout repository loading and delegated target reads. It publishes one protected record containing repository identity, a checked revision, and the verified metadata map. Each update is durably published and read back before verification can proceed. A failed or uncertain publication poisons that store instance until a new invocation reobserves protected state.

Root rotation must publish the new trusted root and any required timestamp/snapshot invalidations in one metadata batch. Separate root replacement and invalidation writes leave a crash window in which a new root can coexist with stale version floors. Corrupt persisted time evidence must refuse refresh, never reset the clock floor silently.

Tough currently exposes a filesystem path for its datastore and no native storage callback. The proposed dependency change is a narrow storage hook and atomic batch operation, retaining Tough's verification code. Zup's shared state projection would use the existing private-directory capability for user scopes and the Windows machine-directory capability for machine scope. Native publication guarantees remain platform-specific; process-termination tests alone cannot establish power-loss durability.

## Persistent-root creation

Persistent data has two declarations. An ordinary path is owned by the
installation principal and can explicitly grant selected service identities
access. A service-state declaration selects the native persistent state facility
for one logical service:

```toml
[[install.data]]
id = "settings"
path = "${location.user_data}/${app.id}"

[[install.data]]
id = "database"
service = "worker"
```

`PersistentDirectory` owns a `PersistentLocation::Path { path, access }` or
`PersistentLocation::Service { service }`. Path access is either the installation
principal or an explicit nonempty set of logical services. The data declaration
is the sole authoring authority for those relationships; services do not repeat
the same ownership references. The planner resolves service identities and adds
the native configuration and activation dependencies.

Filesystem-backed roots retain creator evidence and native object identity.
Their initial service writer policy is immutable. Grant access and inheritance
before exposing a fresh root; later operations verify it without traversing or
rewriting mutable descendants. Windows grants exact service SIDs. Linux static
service grants preserve the root's installer authority and use exact access and
default ACLs. An arbitrary path cannot receive a transient DynamicUser UID.

Systemd DynamicUser state uses `StateDirectory`. Its ownership evidence is a
protected installation/service registration claim, including the exact native
state-directory name. A stopped service may not have created a physical root;
the record must not invent an object identity. Systemd alone provisions and
manages that root. Native and filesystem evidence are variants of the same
persistent ownership model, executed and recovered through the shared graph.

This replaces the smaller alternative of allowing only explicit paths and
refusing DynamicUser state. Native state is necessary for ordinary service DX;
it does not introduce an application runtime or a permission migration engine.

Persistent roots use protected Requested and Created checkpoints. Requested binds
an unpredictable creation token to the transaction, logical resource and path;
Created also records the native directory identity. Linux creates an exclusive
sibling staging directory and publishes it by no-replace rename. An interruption
between mkdir and its identity checkpoint remains ambiguous.

Windows exclusively creates the final private root with its token in the same
NtCreateFile EA request. Before the native-ID checkpoint, recovery requires an
exact EA readback and private owner policy. Unsupported EAs, token mismatch and
foreign roots are refused. Parent handles exclude metadata writers while this
operation runs. The EA is creation evidence within the installation's principal
boundary, not an application runtime dependency or a defense against that same
principal. After identity publication, mutable descendants and EA changes do not
change ownership; replacement of the root itself does.

The privileged Windows preparation envelope is authenticated by its complete
serialized digest. Its typed operation carries either deployment intent and a
requested graph, or only a recovery transaction identity. Recovery obtains the
graph from the protected journal. The worker independently replans deployment,
returns the actual graph, and waits for its matching Execute digest before
creating a transaction journal. Cancellation at Prepared returns a typed result
without application publication or a new journal.

### Retiring retained deployments

Snapshot retirement must publish through the installation record before any
irreversible payload cleanup. Cleanup effects belong to the same typed graph
and journal, after that commit point. Recovery observes uncertain deletion;
it never compensates a committed retirement by restoring obsolete roots.
Retired payload evidence remains protected until cleanup is verified. Busy or
ambiguous consumers defer deletion and remain visible in storage inspection.
A persistent-data origin journal authenticates that resource, without keeping
unrelated payloads from its transaction alive forever.

Native image fences can close admission of direct native executable starts,
but they cannot establish the absence of arbitrary external delayed readers.
Automatic reclamation therefore needs an explicit application access contract
and native enforcement of its remaining execution boundary. The default must
retain instances when that proof is unavailable. PID enumeration and the end of
a main process are insufficient evidence.

## Windows service process scope

SCM main-process exit and Restart Manager observations do not establish that service descendants have stopped. Restart Manager reports current resource users, and SCM reports the
service process. Neither closes future file admission by a surviving worker.
See [RmGetList](https://learn.microsoft.com/en-us/windows/win32/api/restartmanager/nf-restartmanager-rmgetlist)
and [QueryServiceStatusEx](https://learn.microsoft.com/en-us/windows/win32/api/winsvc/nf-winsvc-queryservicestatusex).

The service contract uses an application-owned native Job Object for SCM service
scope. The service joins before reporting Running or creating workers, prohibits
breakaway, configures kill-on-close, and retains only restricted, noninheritable
job handles. Zup observes membership of the exact SCM process, validates the
job's access policy and limits, and pins a query handle during quiescence and
related fixed mutations. It releases an empty old job before restarting the
service, so the new process can create its own scope. The service creates its
scope after reboot without Zup.

The job name is derived from installation and logical service identity. It is a
locator, not durable object identity. Authenticated native observations belong in
the operation plan and receipts. Recovery needs the original process membership,
immutable job policy, the exact closed service gate, and fresh native observation.
Missing evidence must produce recovery-required rather than inferred quiescence.
A boot boundary must use a native boot identity, not elapsed time.

Authoring uses `ServiceExecution::WindowsJob` and a whole argument reference
such as `${job.worker}`. Native cgroup scopes remain the Linux default.
An uncontained SCM service must receive an explicit refusal or a verified reboot
boundary. A bare single-process assertion does not create native containment.
The contract excludes external broker launches and job-handle escape; it does not
claim control of arbitrary application side effects or administrator actions.

The final job DACL grants SYSTEM, Administrators, and the exact service SID only
query, terminate, READ_CONTROL, and SYNCHRONIZE. OWNER_RIGHTS grants READ_CONTROL
and suppresses implicit owner mutation rights. Only the creator's temporary
initial handle can assign the main process and establish limits. It must be
restricted and closed before application work. Native Windows VM experiments
verified membership, owner and foreign same-account mutation denial, descendant
drain, and last-handle termination.

Windows scope evidence carries the process telemetry loader boot sequence.
Recovery requires a strictly later sequence and changed independently verified
SCM identity before using a boot boundary. The native telemetry query is
capability-tested against the documented header, current PID, and process
creation time. Unsupported layouts refuse service recovery. The boot-environment
GUID is not a boot-session token and is never used for this authority.

An uncertain failed start closes its already-authorized native startup gate.
Without an original bound scope or protected empty receipt, it preserves recovery
material and requires a later boot before executable compensation. A stopped
main process alone never authorizes replacement.

See [Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects),
[Job Object security](https://learn.microsoft.com/en-us/windows/win32/procthread/job-object-security-and-access-rights),
and [process telemetry](https://learn.microsoft.com/en-us/windows/win32/devnotes/process_telemetry_id_information_type).
