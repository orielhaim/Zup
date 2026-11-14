# Generation preparation retirement

`GenerationStorage` owns preparation creation, retirement and collection. The
protected materialization record is the only retirement journal. There is no
directory-name scanner, background service or separate preparation GC store.
Application and maintenance generations use the same implementation in both
installation scopes.

## Ownership and transitions

The originating protected transaction must authorize the exact materialization,
installation and privilege. Native adapters also authenticate the state authority
and installation namespace. A staging name alone grants no authority.

1. `Reserved` durably reserves one random staging name before creating it. The
   record's `object` identifies the namespace in this state. Recovery requires
   that same namespace and an absent or empty, private, real directory at the
   reserved name. A nonempty unreceipted directory is refused. Payload writes
   cannot start in this state.
2. Creation is synchronized, its exact native root identity is recorded, and the
   state becomes `Preparing`. From here, `object` identifies the content root.
   Recovery never adopts a replacement for a recorded content root.
3. An interrupted preparation that cannot be verified becomes `Retired`. The
   same durable update reserves its replacement. It happens before creating the
   replacement directory, so failed metadata publication cannot leak a new tree.
4. Collection synchronizes the files and directory membership, then records
   `Deleting` with an inventory of the private root's native
   directory identities and each regular file's identity, size and digest. This
   inventory includes partial native staging files, which need not appear in the
   final payload descriptor. Private root authority owns those temporary members;
   public payload membership does not authorize this operation.
5. Deletion accepts only unchanged inventory members. Missing members are allowed
   once `Deleting` is durable. After root removal, the parent is synchronized and
   the name is observed absent before the retirement entry is durably removed.

Preparation reservation does not infer a content identity before one exists. An
empty directory at a reserved name can complete an interrupted creation only
under the recorded namespace and private authority. Its native receipt is durable
before any bytes are written. This authority assumes that processes with the
installation owner's credentials, or machine administrator authority, are trusted.

The current root proceeds through `Prepared`, `Publishing` and `Sealed` as before.
Collection never traverses that root, a published root, or a retained rollback
generation. A sealed generation still uses the existing sealed generation
collector and its declared reader contract.

## Coordination and native deletion

An installation coordinator lock covers generation apply, verify, reconcile,
compensation retention and sealed generation collection. Windows stores the lock
in the protected installation materialization namespace. Linux uses the existing
protected resource lock directory. Contention is reported without mutating the
materialization record.

The shared transaction core inventories and removes through held directory
capabilities. It does not use a privileged pathname recursive remover. Each
native adapter verifies private ownership and permissions before opening members,
rejects links and special entries, and checks the expected identity again at
removal. Files with multiple hardlinks are refused. Inventory traversal is bounded
to 128 nested separators and 100,000 members.

Windows rejects reparse points and alternate streams. File deletion uses an
exclusive native handle, exact identity and digest readback, and handle-based
disposition. Conflicting file readers cause a sharing refusal. Machine mutations
use the existing volume flush barrier; user mutations use `private-fs` durability
operations and subsequent namespace observation. A failed barrier leaves the
retirement pending. Directory capabilities are released before deletion readback
so a delete-pending object cannot be mistaken for completed collection.

Linux uses no-follow directory opens, `statx` mount IDs, held identities,
`unlinkat`, and directory `fsync`. Mount crossings, including same-device bind
mounts, and missing mount-ID support are refused. File removal holds a write open
to exclude an executing native image through `ETXTBSY`. Private staging has never
been published to application consumers; arbitrary same-owner read handles are
not an independently tracked reader contract. Named unlink relies on the protected
parent and coordinator to exclude untrusted namespace mutation.

## Recovery and seals

Apply, reconcile, verification and compensation retention retry pending retirement
work. Collection runs before another preparation can be retired. Failure retains
the evidence and returns an observable error; retry does not need installer bytes
to collect a retired tree. Completing an unfinished materialization still needs
its source, while verifying an already sealed generation does not.

An absent root in `Retired` is a refusal because no deletion was authorized. An
absent root in `Deleting` completes only after parent synchronization and absence
readback. A changed root, extra member, changed file or changed private access is
refused. Recovery does not search for moved roots or delete foreign siblings.
An interrupted compaction repeats observation from the durable `Deleting` entry.

Materialization record schema 2 separates the seal from mutable retirement
metadata. `MaterializationSeal` authenticates schema, creator transaction,
specification, namespace identity, content root and verified member tree with the
`zup:materialization-seal:2` domain. Only `Sealed` records have a fingerprint.
Retirement insertion, deletion intent and compaction do not change that seal,
materialization reference, installation ownership or existing generation receipts.
Prototype schema 1 materialization records are rejected, without a migration path.

History collection continues to retain the materialization sidecar while any
retirement remains. Successful compaction removes that existing history blocker.
This lifecycle starts from an authenticated transaction and does not change the
earlier transaction-directory creation window described by F11.

## Verification limits

`fixtures/preparation_retirement.rs` exercises native Windows process crashes,
repeated partial attempts, member and root deletion boundaries, metadata
compaction, changed identities, foreign insertion, junctions, hardlinks, ACLs,
busy readers, coordinator conflicts, maintenance content and source-free sealed
receipt verification. The original generation crash tests now require collection
instead of asserting that partial trees leak.

Linux all-target compilation and Clippy can run from Windows with Zig. Native
Linux tests require a healthy Linux environment. The dispatch environment's WSL
read-only probe failed with account lookup and I/O errors, so it was left intact.
Distinct-token and elevated machine tests, native Linux mount and executable
reader tests, reboot recovery, and sudden power loss still require their native
test environments. Process termination tests do not qualify power-loss durability.
