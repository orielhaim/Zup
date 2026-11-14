# Deployment rebuild audit

Status: research in progress. This is not an implemented architecture.

Checkpoint: `241f9ec4a196b862ce68512e1586948e1e0383ff`.

## Confirmed source boundaries

- `zup-core/src/build_plan.rs` holds build artifacts and target-specific installer content.
- `zup-plan/src/plan_types.rs` holds desired payload files and native resources. Files have destinations, hashes, sizes and executable flags, without generation or persistent placement semantics.
- `zup-core/src/ids.rs` identifies file resources by destination. The replacement must distinguish ownership identity from physical location.
- `zup-plan/src/binding.rs` binds execution effects to desired content and the observed committed transaction. Preserve independent authorization validation when replacing these contracts.
- `zup-transaction/src/plan.rs` validates a graph centered on staging, file mutation, backend operations and service-control phases. Generation materialization, sealing and activation need explicit graph effects.
- `zup-transaction/src/coordinator.rs::reconcile_running` observes uncertain effects and preserves ambiguous outcomes as recovery required. Do not replace it with blind journal replay.
- `zup-transaction/src/record.rs` distinguishes uncertain publication from definite non-publication and requires a directory durability barrier. Retain these semantics.
- `native/openrc/README.md` explicitly requires patched `pidfd-control-v1`. Its necessity remains to be assessed against generation-pinned native service configurations.

## Competing strategies to evaluate

1. In-place placement everywhere retains executable replacement and rollback backup costs. It cannot be the default generation design.
2. Immutable generations everywhere cannot satisfy mandatory physical paths or persistent writable data.
3. Global content storage adds shared authority and garbage-collection complexity. No measured benefit currently justifies making it foundational.
4. Per-installation generations with fixed, persistent and native resources in one graph meet the requested placement requirements. Activation boundaries and platform refusal rules still require experiments.
5. Native package staging provides useful separation of payload preparation and registration, but cannot replace arbitrary fixed placement across both operating systems.

These are design candidates, not experimental findings.

## Primary sources located

- OSTree deployment layout and mutable state: https://github.com/ostreedev/ostree/blob/main/docs/deployment.md
- OSTree atomic deployments: https://github.com/ostreedev/ostree/blob/main/docs/atomic-upgrades.md
- Nix retained generations: https://nix.dev/manual/nix/2.35/command-ref/nix-env/delete-generations.html
- MSIX staging and registration: https://learn.microsoft.com/en-us/windows/msix/package/package-lifetime
- Velopack current-directory replacement and maintenance executable: https://docs.velopack.io/packaging/installer
- Windows handle-relative rename contract: https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_rename_info
- OpenRC supervisor guide: https://github.com/OpenRC/openrc/blob/master/supervise-daemon-guide.md
- OpenRC cgroup lifecycle: https://github.com/OpenRC/openrc/blob/master/user-guide.md

Sources require detailed review and experiments before decisions are final.

## Verification status

Windows baseline started: `cargo nextest run -p zup-core -p zup-plan -p zup-transaction --no-fail-fast`.
No outcome recorded yet. Compilation is not lifecycle proof.
WSL distributions detected: Ubuntu and docker-desktop.
QEMU and VirtualBox were not found on the Windows command search path; VM availability remains unverified.

Baseline outcome: 409 tests passed, zero skipped, 15.872 seconds. This validates existing contracts only, not the requested rewrite.

## Native experiments completed

`python scripts/verify-generation-pinning.py` passed on the Windows host.
`wsl -d Ubuntu -- sh -lc 'cd /mnt/d/Projects/Zup && python3 scripts/verify-generation-pinning.py'` passed under WSL.
Both used real compiled executables. A running executable read generation A's adjacent resource after B was prepared, and B independently read its own resource. Windows refused opening the running A image for write. This proves only the tested generation-pinned lookup pattern. It does not establish arbitrary application compatibility, DLL lookup safety, activation atomicity, engine behavior, or crash durability.

QEMU x64 and ARM64 are available under Ubuntu at `/usr/bin/qemu-system-x86_64` and `/usr/bin/qemu-system-aarch64`. The earlier Windows PATH check did not describe the WSL environment. VM provisioning and guest readiness remain to be checked.

## Dependency investigation

No dependencies added. Primary documentation inspected:

- private-fs 0.2.1 documents retained directory handles, identity and ACL verification, and refusal of namespace escape. Its owner-private permissions are appropriate for private installation state, not a general readable machine payload directory. Machine payload authority needs the existing Windows capability design or an equivalent policy-aware native implementation. https://docs.rs/private-fs/latest/private_fs/
- rustix 1.1.6 exposes openat2 with typed resolve flags. Existing workspace rustix can provide Linux path-resolution restrictions without another crate. https://docs.rs/rustix/latest/rustix/fs/fn.openat2.html
- cap-std 4.0.3 exposes directory-relative filesystem operations. It does not by itself replace Zup's authorization policy; adding a second capability library needs a concrete gap. https://docs.rs/cap-std/latest/cap_std/fs/struct.Dir.html
- atomic-write-file 0.3.1 provides atomic individual-file publication. It cannot make a multi-resource native activation globally atomic. https://docs.rs/atomic-write-file/latest/atomic_write_file/
- object 0.40.0 exposes architecture and import libraries. Existing zup-binary already uses it. Static import inspection cannot prove relocation safety or discover all runtime-loaded resources. https://docs.rs/object/latest/object/read/trait.Object.html
- typed-path 0.13.0 supports host-independent Windows and Unix parsing. Lexical checks still require handle-bound authorization at execution. https://docs.rs/typed-path/latest/typed_path/
- reflink-copy 0.1.32 exposes copying between opened files as well as path APIs. Evaluate only the retained-handle form for privileged materialization. No speed or filesystem-support claim has been tested. https://docs.rs/reflink-copy/latest/reflink_copy/

## Windows documented boundaries

SCM ChangeServiceConfig accepts a fully qualified binary path, with quoting required for spaces. This enables generation-pinned registration but is not atomic with other activation resources. https://learn.microsoft.com/en-us/windows/win32/api/winsvc/nf-winsvc-changeserviceconfigw

Restart Manager can coordinate permitted shutdowns, but excludes critical services and does not support shutdown across sessions. A successful query must not be treated as authority over all possible users of a generation. https://learn.microsoft.com/en-us/windows/win32/rstmgr/about-restart-manager

FILE_RENAME_INFO supports a retained target-directory handle and replacement control. Actual executable locks, reparse behavior and durability still need the platform's validated mutation path and explicit recovery. https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_rename_info

## Linux documented boundaries

Luna's focused primary-source review found that openat2 RESOLVE_BENEATH confines lookup to its retained directory, with EAGAIN possible when a rename race prevents proving confinement. RESOLVE_NO_SYMLINKS rejects symlinks throughout lookup. https://man7.org/linux/man-pages/man2/openat2.2.html

RENAME_EXCHANGE atomically exchanges existing names. File fsync does not persist the directory entry; the directory needs its own fsync. These are documented guarantees, not power-loss tests on Zup's supported filesystems. https://man7.org/linux/man-pages/man2/renameat2.2.html https://man7.org/linux/man-pages/man2/fsync.2.html

OpenRC cgroup cleanup is configurable and rc_cgroup_cleanup defaults to NO. cgroup.kill may be used when available; otherwise signaling and escalation depend on configuration. A missing main PID cannot establish an empty cgroup. The existing controller verifier explicitly disables automatic cleanup, so its results would not prove stock cleanup behavior. https://github.com/OpenRC/openrc/blob/master/user-guide.md https://github.com/OpenRC/openrc/blob/master/etc/rc.conf

## Activation decision still requiring an experiment

Direct generation-pinned paths passed the native adjacent-resource probe. Public mutable aliases remain unproven, particularly Windows junction execution where executable-path reporting may retain the public path. Do not make such aliases the default until tested. Native registrations can point directly at generation paths; a stable CLI path needs its own explicit compatibility and platform contract. No mandatory Zup launcher may be introduced to avoid this constraint.

## Windows alias experiment outcome

`python scripts/probe-windows-alias.py` completed on the Windows host. The fixture launched directly from `a/app.exe` reported `a/app.exe` and read resource `a` after changing `current` from A to B. Launched through `current/app.exe`, it reported `current/app.exe` and read resource `b` after that same switch. The running A process therefore mixed its code with B's adjacent resource in the alias case. This is a concrete counterexample to unconditional Windows junction activation. It does not test DLL loading or prove compatibility of any other application. Machine-readable result: `target/generation-alias-probe.json`.

Provisional rule: native registrations use generation-pinned executable paths. Mutable entrypoint aliases require an explicit application-authored path-resolution compatibility contract. Where that contract cannot be satisfied, fixed placement must be declared or the requested alias refused. Do not infer compatibility from binary imports or absence of suspicious strings.

## Additional ecosystem findings

Squirrel's documented default shortcuts target Update.exe, which conflicts with Zup's no-mandatory-launcher requirement. Keep the generation isolation idea, not that execution dependency. https://github.com/Squirrel/Squirrel.Windows/blob/develop/docs/using/install-process.md

s6-supervise runs a native supervision tree and normally persists for service lifetime. Its existence is compatible with OS-native supervision, but adopting another manager would add a host prerequisite; no demonstrated requirement currently justifies requiring it. The native comparison below tests this on a disposable Alpine guest. https://www.skarnet.org/software/s6/s6-supervise.html

Windows documents ReFS block cloning with independent writes after cloning. Do not assume this facility is available on the Windows host's NTFS deployment volume. https://learn.microsoft.com/en-us/windows/win32/fileio/block-cloning

Disposable guest disk images exist under the WSL Zup verification VM cache for stock Alpine, Alpine s6, patched OpenRC x64/ARM64 and Ubuntu. No running QEMU process was reported by the inventory command. Guest ownership, snapshots, network ports and readiness must be established before starting or modifying them.

## Fixed-file native identity experiment

`python scripts/probe-windows-fixed-publication.py` ran on the Windows host's NTFS system volume. Copying the prior file produced a different file ID. ReplaceFileW with a backup preserved the original file ID in the backup; restoring it by rename preserved that ID. A handle denying delete sharing caused error 32 with the target unchanged. Native handle-relative no-replace renames preserved original identity and refused an occupied foreign destination with status `c0000035`. These are tested namespace and identity observations, not power-loss or arbitrary-process quiescence proofs.

The current Windows fixed executor copies both backups and staged publication, and its volume argument only names a directory under the state volume. It therefore needs replacement before fixed-file native identity can safely become authoritative. Linux already uses exchange or quarantine renames, but its ownership and ordinary receipts still lack persisted native identity. Exact preconditions now bind the observed object, and interrupted replacement checks that object in the old-content slot.

A second experiment retained the original through a handle-relative hard link, then attempted atomic replacement with `FileRenameInformationEx`. Replacement succeeded only when the original handle allowed delete sharing. Under that same sharing policy, another handle could replace the public name with a foreign object while the original guard remained open. This strategy preserves backup identity but does not exclude namespace substitution. A read/write/delete guard was separately refused with error 32 while an actual PE fixture was running. Exact-object no-replace moves remain the conservative candidate; their temporary absence window requires an explicit activation boundary.

Primary contracts: [ReplaceFileW](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-replacefilew), [FILE_RENAME_INFO](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_rename_info).

## Stock supervisor and kernel containment verification

`scripts/probe-linux-supervisors.py` created fresh Alpine 3.24.2 QEMU guests with
OpenRC 0.63.2-r0, s6 2.15.0.0-r0 and Linux 6.18.54-0-virt. Both native supervisors
passed start, forced respawn, generation A-to-B stop/start, and an actual guest
reboot. No Zup executable was installed in those guests. Editing the service
configuration to B while A was running still respawned A until native stop/start.
Configured and running generations are distinct facts.

Both supervisors left escaped descendants after ordinary stop and after their
controller disappeared. Enabling OpenRC cgroup v2 put supervise-daemon workloads
in an isolated `openrc.<service>` group; s6 workloads remained in the root group.
The packaged OpenRC cgroup helper checked a different namespace from the group it
created and returned without cleanup. Current upstream source differs from that
packaged implementation.

The kernel cleanup probe retained and revalidated the isolated cgroup's native
identity, wrote `cgroup.kill`, and observed `cgroup.events` with `populated 0`.
It removed the stock supervise-daemon fixture's escaped descendants after both
ordinary stop and controller loss. The s6 root-group case was refused. This does
not establish startup inhibition, concurrent manager exclusion, transaction
recovery, ARM64 behavior, or power-loss durability. The production OpenRC patch
remains until a native replacement satisfies those additional requirements.

Results: `target/native-supervisor-probe.json`,
`target/native-supervisor-containment-probe.json`, and
`target/native-supervisor-kernel-cleanup-probe.json`. Each guest was shut down.

## Logical plugin files and exact observed preconditions

Plugin generated files now declare a stable ID in WIT and the SDK. Ownership keys
include the authenticated plugin ID and logical file ID, independently of path
resolution. The prior path-derived key failed actual CLI planning after a
location template resolved. Generated source names now derive from logical keys.

Exact fixed-file preconditions include the observed native object identity as
well as content. Actual Windows and Linux tests refuse equal-byte replacement by
a different object between observation and execution. This closes that planning
check; it does not supply the missing retained instance/receipt protocol or prove
namespace exclusion across every mutation.

## Stock OpenRC admission

The pinned OpenRC source acquires its service lock before executing the start
action's shell, which opens the service definition by path. Lock contention exits
through `ewarnx`; it does not continue the start. `--nodeps` can instead return
success without starting anything. See [start dispatch](https://github.com/OpenRC/openrc/blob/b3a99e01c8bda7e5c2b9a8975a6d36ec3eff370e/src/openrc-run/openrc-run.c)
and [fatal warning behavior](https://github.com/OpenRC/openrc/blob/b3a99e01c8bda7e5c2b9a8975a6d36ec3eff370e/src/libeinfo/libeinfo.c).

An actual Alpine 3.24.2 guest with OpenRC 0.63.2-r0 confirmed that an externally
held lock blocked normal and `--nodeps` starts. Neither request unlinked the held
entry. Unblocked baselines started successfully. A musl test hook paused a real
request before its lock call. After protected lock acquisition and atomic source
replacement, the resumed request executed definition B rather than A.

The candidate protocol must acquire nonblocking and revalidate the entry's inode.
OpenRC unlinks the entry when releasing its lock and when marking several service
states. Zup must never wait on an unlinked inode or run those bookkeeping mutators
while relying on its retained guard. See [lock lifecycle](https://github.com/OpenRC/openrc/blob/b3a99e01c8bda7e5c2b9a8975a6d36ec3eff370e/src/shared/misc.c)
and [service state publication](https://github.com/OpenRC/openrc/blob/b3a99e01c8bda7e5c2b9a8975a6d36ec3eff370e/src/librc/librc.c).

`scripts/probe-openrc-admission.py` records the experiment in
`target/openrc-admission-probe.json`. This establishes the tested admission and
script-read behavior, not complete lifecycle recovery, power-loss durability, or
authority validation for a production Rust guard. The custom supervisor patch
remains until the complete native replacement is implemented and verified.
