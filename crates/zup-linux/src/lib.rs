//! Linux backend for zup.
//!
//! A sibling of `zup-windows`, not a layer above it and not a copy of it. Each
//! backend owns the mechanisms of exactly one operating system, and the portable
//! crates below them own everything else.
//!
//! What is here in this phase is the part of a Linux backend that is
//! well-defined independently of a working installer: what this host is, where a
//! Linux target's paths land, where a scope's persistent state belongs, and what
//! a build source is allowed to be. What is deliberately absent is the part that
//! depends on work not done yet - desktop integration, services, privilege
//! separation, a worker transport. An absent concept is reported as absent at
//! the capability boundary rather than answered with a function that returns
//! `Unsupported`.
//!
//! Every Linux-specific idea in this crate is a *lowering* of a portable one:
//! [`host`] answers zup's existing selection model with this machine's identity,
//! [`lowering`] turns a portable [`TargetPath`] into a host path, [`state`] places
//! a scope's state where the XDG model puts it, and [`source_policy`] decides
//! what a build may read. None of them introduces a Linux-shaped type to the
//! portable model, which is what lets both backends be used by the same caller.
//!
//! ```text
//! portable intent
//!        ↓
//! native lowering      ← this crate
//!        ↓
//! native mechanism
//! ```
//!
//! # Where this crate runs
//!
//! Every mechanism module is gated on the target operating system, so on any
//! other host the crate compiles to nothing and exports nothing. That is
//! deliberate rather than a limitation: a backend that answers on a platform
//! it has no mechanisms for would be a backend whose answers are guesses, and
//! the honest thing for a host without this backend is to have no symbols to
//! call. The package matrix records the same fact - `zup-linux` is verified
//! where Linux can be built - so a CI job that wants to prove anything about
//! this crate has to be a Linux job.
//!
//! The one exception is [`carrier`]. Composing and opening a carrier is bytes,
//! not mechanism: a Windows build host composes the Linux installer it cannot
//! run, and inspects one the same way, so the carrier compiles everywhere. Only
//! the Unix executable bit it preserves is platform-gated, and its absence on a
//! non-Unix host changes no byte of the artifact.

#![forbid(unsafe_code)]

#[cfg(target_os = "linux")]
mod capabilities;
mod carrier;
#[cfg(target_os = "linux")]
mod desktop;
#[cfg(target_os = "linux")]
mod elevate;
#[cfg(target_os = "linux")]
mod executor;
#[cfg(target_os = "linux")]
mod fs;
#[cfg(target_os = "linux")]
mod host;
#[cfg(target_os = "linux")]
mod input;
#[cfg(target_os = "linux")]
mod integration;
#[cfg(target_os = "linux")]
mod ledger;
#[cfg(target_os = "linux")]
mod locations;
#[cfg(target_os = "linux")]
mod lowering;
#[cfg(target_os = "linux")]
mod machine;
#[cfg(target_os = "linux")]
mod mime;
#[cfg(target_os = "linux")]
mod pkexec;
#[cfg(target_os = "linux")]
mod refresh;
#[cfg(target_os = "linux")]
mod resolve;
#[cfg(target_os = "linux")]
mod run;
#[cfg(target_os = "linux")]
mod snapshot;
#[cfg(target_os = "linux")]
mod socket;
#[cfg(target_os = "linux")]
mod source_policy;
#[cfg(target_os = "linux")]
mod state;
#[cfg(all(target_os = "linux", feature = "test-support"))]
pub mod test_support;
#[cfg(target_os = "linux")]
mod worker;

#[cfg(target_os = "linux")]
pub use capabilities::{LinuxCapabilityError, validate_target_plan};
pub use carrier::{CARRIER_MAGIC, CARRIER_VERSION, Carrier, CarrierError, CarrierFooter, compose};
#[cfg(target_os = "linux")]
pub use executor::{FileIntent, FileWork, LinuxFileExecutor, LinuxFileExecutorError};
#[cfg(target_os = "linux")]
pub use fs::{
    EXECUTABLE_PAYLOAD_MODE, EntryKind, FileSystemError, OwnedDirectory, PAYLOAD_FILE_MODE,
    STATE_DIRECTORY_MODE, STATE_FILE_MODE, refuse_symlink_ancestors, sync_directory,
};
#[cfg(target_os = "linux")]
pub use host::{
    HostError, additional_architectures, host_execution, host_version, native_architecture,
};
#[cfg(target_os = "linux")]
pub use input::{LinuxInputError, compile_execution_plan};
#[cfg(target_os = "linux")]
pub use ledger::{LinuxLedgerError, LinuxLedgerStore};
#[cfg(target_os = "linux")]
pub use locations::{
    LinuxInstallLocationResolver, LinuxLocationError, user_data_home, user_data_home_in,
    user_programs_root, user_programs_root_in,
};
#[cfg(target_os = "linux")]
pub use lowering::{
    LinuxPathLoweringError, linux_target_path, target_path_from_host, to_host_path,
};
#[cfg(target_os = "linux")]
pub use machine::{
    MACHINE_LOCK_FILE_MODE, MACHINE_PRIVATE_DIR_MODE, MACHINE_PRIVATE_FILE_MODE,
    MACHINE_PROGRAMS_ROOT, MACHINE_PUBLIC_FILE_MODE, MACHINE_SHARED_DATA_ROOT,
    MACHINE_STATE_DIR_MODE, MACHINE_STATE_ROOT, MachineDestination, MachinePathPolicyError,
    MachineRoots, MachineStateError, authorize_machine_destination,
    authorize_machine_install_directory, ensure_machine_state_root, normalize_state_modes,
    verify_ledger_trust, verify_machine_hierarchy, verify_machine_structure,
    verify_trusted_state_file,
};
#[cfg(target_os = "linux")]
pub use pkexec::{
    LaunchOutcome, PkexecError, PkexecLauncher, SystemPkexec, WorkerChild, map_launch,
};
#[cfg(target_os = "linux")]
pub use resolve::{LinuxResolveError, resolve_target, resolve_target_for, resolve_target_with};
#[cfg(target_os = "linux")]
pub use run::{
    LinuxAction, LinuxOutcome, LinuxRunError, LinuxRunRequest, recover_transaction, run,
};
#[cfg(target_os = "linux")]
pub use snapshot::snapshot_target;
#[cfg(target_os = "linux")]
pub use socket::{
    FRAME_TIMEOUT, HANDSHAKE_TIMEOUT, PeerIdentity, PeerPin, Rendezvous, SocketError,
    peer_identity, pin_peer, worker_socket_path,
};
#[cfg(target_os = "linux")]
pub use source_policy::{LinuxSourceFilePolicy, SourceEntryKind};
#[cfg(target_os = "linux")]
pub use state::{LinuxStateError, machine_state_root, state_root, user_state_root};
#[cfg(target_os = "linux")]
pub use worker::{EXECUTE_TIMEOUT, FilePin, WorkerContext, WorkerError, run_worker_mode};
