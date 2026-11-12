# Durable transaction journal

Journal schema and storage format **4** deliberately reject older prototype journals.
Workers must validate `JOURNAL_SCHEMA` as well as their execution-format contract.
There is no snapshot-format fallback. `TransactionStore::load` still reconstructs
the complete validated `TransactionRecord` for recovery, publication and audit.

## Representation and authority

| Member                                   | Contents                                                                                                                                                           |
| ---------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `transaction.lock`                       | F11 seal of the transaction, namespace, directory and lock native identities.                                                                                      |
| `transaction.json`                       | Immutable initial record, including the complete plan and its fingerprint.                                                                                         |
| `head.json`                              | Format, transaction ID, intent digest, active slot, checkpoint digest/base revision, current revision, committed log length, chain digest and native log identity. |
| `events-a.log`, `events-b.log`           | Alternating logs of length-prefixed typed transitions. Only the active head's exact prefix is authoritative.                                                       |
| `checkpoint-a.json`, `checkpoint-b.json` | Alternating complete mutable states, without another copy of the plan.                                                                                             |

The existing protected namespace authenticates these files. SHA-256 detects damage
and binds records together; it is not a signature against a hostile administrator or
the owning user. Neither the old snapshot nor this format protects against rollback
of the entire protected volume by its administrator.

`JournalLease` keeps the original OS lock and directory capabilities. Every file
operation verifies the F11 lock seal and the namespace's exact directory identity.
Private files retain ownership, access-policy, no-follow and mount checks. Windows
machine files retain the Administrators policy and handle-relative MachineRoot
operations. Log replacement with identical bytes fails its native identity check.
There is no new lock namespace, path-based database connection or daemon.

F11 creation still reserves the exact initial record and native identities outside
discovery. It publishes intent, an empty first log and the initial head before the
initialized namespace becomes discoverable. Interrupted reserved initialization
can finish. Published missing members, foreign slots and substituted locks remain
errors; an initial creation reservation never restores a damaged published journal.

## Commit and recovery

`commit_changes(id, expected_revision, changes)` commits a group of typed
`TransactionChange` values. Node and phase changes must follow their state machines.
The authoritative current revision must equal the expected revision. Exhaustion of
the revision counter fails closed. Snapshot CAS is translated to the same typed
changes and rejects any change to immutable identity or plan. It does not rewrite
the initial record. The coordinator's node and phase paths update their local state
using the returned revision and timestamp, without cloning the complete plan.

While holding the sealed lease, an ordinary commit:

1. Reads and validates the small head and checks the expected revision.
2. Truncates only bytes beyond the head's committed length, if any.
3. Appends an eight-byte little-endian payload length and a JSON event containing
   the next revision, timestamp and typed changes. A frame is limited to 64 MiB.
4. Flushes the log handle and rechecks its native identity.
5. Atomically replaces and durably publishes `head.json` through the existing
   native publication implementation. The head binds the previous chain digest,
   frame length and payload using a versioned SHA-256 domain.
6. Returns acknowledgement only after publication succeeds. Native effects follow
   durable intent; receipts and verification follow their corresponding effects.

The head replacement is the visibility point. Its successful durable publication
is the acknowledgement barrier. If a write, flush or publication fails, the caller
does not perform the next native effect. The cache is discarded on every such
error. A restart may observe either the old head or the new head. Recovery loads
that head, validates the exact committed prefix, and completes log and namespace
flushes before returning it. A visible intent with a lost acknowledgement is
conservatively retained.

Every committed frame must parse, continue the revision sequence, follow valid
transitions and leave a valid record. The final chain digest and revision must
match the head. A missing checkpoint, short committed log, changed intent, bad
checksum or discontinuous/replayed revision fails closed. Recovery never searches
for an alternative checkpoint or adopts a new file as history. Bytes strictly past
the authoritative head cannot have acknowledged a state change. Readers ignore
those bytes; a later writer truncates them under the same lease. Corruption inside
the committed prefix is never repaired by truncation.

A store caches at most one reconstructed transaction. Each access still acquires
the native lease and reads the head. Native identity, length and modification-time
stamps for intent, active log and checkpoint invalidate the cache on observed file
changes. A changed head or stamp triggers full authenticated replay. The cache
assumes that writers within the protected authority follow the lease contract.

## Bounded compaction and retention

The replay budget is the larger of the initial intent size, the largest checkpoint
seen by the cached writer, and 64 KiB. Cold loading resets the checkpoint budget to
the active checkpoint size. Before another frame would exceed twice that budget, the writer
publishes the complete **next mutable state** to the inactive checkpoint, publishes
an empty inactive log, and then atomically publishes a head selecting both. The
current active pair remains untouched until the new head is durable. A crash before
the head selects the checkpoint leaves the previous state authoritative; a crash
after it selects the complete new state. Missing evidence never causes fallback.

The next compaction reuses the other slot. There are at most seven journal files,
independent of retry count. Disk and replay bytes are bounded by the immutable
intent and recent complete mutable states, rather than the lifetime number of
repairs. Compaction drops obsolete transition versions, retaining **every field of
the current record**, including compensated receipts, accepted recovery decisions,
publication evidence and deferred cleanup receipts. It never collects retained
sources, generation preparations, installation history or other sidecars.

History retirement enumerates and seals all present journal members in its cleanup
receipt, validates the reconstructed source record, and verifies member identity
and content before removal. Unknown entries defer collection. Existing reachability
rules continue to protect F05 source-free recovery and F08–F12 lifecycle evidence.

## Complexity and design choice

Let `P` be the immutable intent size, `S` the retained mutable state size, `E` the
total encoded changes, and `T` the number of commits. Warm sequential operation
reads `O(P + T)` metadata bytes and writes `O(P + E + T)` bytes amortized, including
checkpoint writes. A growing receipt is charged at its actual encoded size.
Each checkpoint costs `O(S)` after `O(max(P, S, 64 KiB))` new log bytes. Cold replay
reads `O(P + S)` bounded bytes. Memory is `O(P + S + largest frame)` per cached
transaction, with temporary mutable-state copies during checkpoint serialization.

These are byte-complexity claims. Full mutable-state validation still scans the
record after each transition, including replay. CPU time can therefore remain
superlinear with node count. Immutable graph validation and plan fingerprinting
occur at creation/cold-load boundaries, rather than every warm node transition.

Independent state shards require another atomic manifest to commit related node,
reservation and publication changes together. SQLite would need an appropriate
capability-relative VFS to preserve the native directory and lock authority used
here. The bounded log uses existing atomic publication and protected file handles,
adds no dependency, and implements only this transaction's typed state protocol.

Native flush guarantees depend on the filesystem and storage device. Process-abort
and restart tests do not establish sudden-power-loss durability or protection
against a device that lies about completed flushes. See the
[F13 measurements and validation](../verification/f13-journal.md).
