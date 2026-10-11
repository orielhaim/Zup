#![deny(unsafe_code)]

//! Linux installation backend and Linux installer composition.
//!
//! Two responsibilities share this crate, and the build host decides which one
//! compiles:
//!
//! - **Composition** turns a runtime template and a portable package into an
//!   installer for a Linux target by appending the package and a footer. It is
//!   plain file I/O, so it runs on any host. The target plan is validated with
//!   [`resolve_target`], which answers user roots from fixed build roots
//!   instead of the build host's environment. These modules have no `cfg`
//!   gate.
//! - **The native runtime** installs, repairs, and removes software through
//!   filesystem ownership, systemd, D-Bus, and a privileged worker. It
//!   executes only on Linux, and every module it needs is gated on
//!   `cfg(target_os = "linux")`.

mod carrier;
#[cfg(target_os = "linux")]
mod elevate;
mod error;
#[cfg(target_os = "linux")]
mod executor;
#[cfg(target_os = "linux")]
mod fs;
#[cfg(target_os = "linux")]
mod input;
mod integration;
#[cfg(target_os = "linux")]
mod ledger;
mod locations;
mod lowering;
#[cfg(target_os = "linux")]
mod machine;
#[cfg(target_os = "linux")]
mod paths;
#[cfg(target_os = "linux")]
mod pkexec;
#[cfg(target_os = "linux")]
mod refresh;
mod resolve;
#[cfg(target_os = "linux")]
mod run;
#[cfg(target_os = "linux")]
mod service_exec;
#[cfg(target_os = "linux")]
mod service_ops;
mod services;
#[cfg(target_os = "linux")]
mod socket;
#[cfg(target_os = "linux")]
mod systemd;
#[cfg(all(target_os = "linux", feature = "test-support"))]
#[allow(unsafe_code)]
pub mod test_support;
#[cfg(target_os = "linux")]
mod worker;

pub use carrier::{CARRIER_MAGIC, CARRIER_VERSION, Carrier, CarrierError, CarrierFooter, compose};
pub use error::{ExecError, IpcError, PathError, PlanError};
#[cfg(target_os = "linux")]
pub use executor::{FileIntent, FileWork, LinuxFileExecutor, ServiceSupport, snapshot_target};
#[cfg(target_os = "linux")]
pub use fs::{
    EXECUTABLE_PAYLOAD_MODE, EntryKind, OwnedDirectory, PAYLOAD_FILE_MODE, STATE_DIRECTORY_MODE,
    STATE_FILE_MODE, refuse_symlink_ancestors, sync_directory,
};
#[cfg(target_os = "linux")]
pub use input::{
    ServiceCompilation, compile_execution_plan, compile_machine_execution_plan,
    ledger_has_services, requires_service_manager, snapshot_services,
};
#[cfg(target_os = "linux")]
pub use ledger::LinuxLedgerStore;
pub use locations::{
    LinuxInstallLocationResolver, MACHINE_PROGRAMS_ROOT, MACHINE_SHARED_DATA_ROOT,
    MACHINE_STATE_ROOT, MachineRoots, user_data_home, user_data_home_in, user_programs_root,
    user_programs_root_in,
};
pub use lowering::{linux_target_path, target_path_from_host, to_host_path};
#[cfg(target_os = "linux")]
pub use machine::{
    MACHINE_LOCK_FILE_MODE, MACHINE_PRIVATE_DIR_MODE, MACHINE_PRIVATE_FILE_MODE,
    MACHINE_PUBLIC_FILE_MODE, MACHINE_STATE_DIR_MODE, MachineDestination, SYSTEMD_UNIT_FILE_MODE,
    SystemdRoots, authorize_machine_destination, authorize_machine_install_directory,
    authorize_systemd_unit, ensure_machine_state_root, normalize_state_modes,
    resolve_machine_state_root, verify_ledger_trust, verify_machine_hierarchy,
    verify_machine_structure, verify_trusted_state_file,
};
#[cfg(target_os = "linux")]
pub use paths::{
    LinuxSourceFilePolicy, SourceEntryKind, additional_architectures, host_execution, host_version,
    machine_state_root, native_architecture, state_root, user_state_root,
};
#[cfg(target_os = "linux")]
pub use pkexec::{LaunchOutcome, PkexecLauncher, SystemPkexec, WorkerChild, map_launch};
pub use resolve::{resolve_target, validate_target_plan};
#[cfg(target_os = "linux")]
pub use run::{LinuxAction, LinuxOutcome, LinuxRunRequest, recover_transaction, run};
#[cfg(target_os = "linux")]
pub use service_ops::{
    MAX_UNIT_BYTES, SERVICE_BACKEND_PREFIX, ServicePayload, ServiceReceipt, admin_override_dir,
    backend_id_for_unit, backend_key_for_unit, check_collisions, check_no_full_override,
    decode_payload, desired_policy, encode_payload, ledger_key_for_payload, load_path_dirs,
    policy_for_state, read_canonical_source, refuse_source_symlink, validate_changes,
    validate_executable, validate_executable_live,
};
pub use services::{
    DesiredService, MINIMUM_SYSTEMD_VERSION, SERVICE_TYPE, SYSTEMD_UNIT_DIR, UNIT_PREFIX,
    WANTED_BY, parse_manager_version,
};
#[cfg(target_os = "linux")]
pub use socket::{
    FRAME_TIMEOUT, HANDSHAKE_TIMEOUT, PeerIdentity, PeerPin, Rendezvous, peer_alive, peer_identity,
    pin_peer,
};
#[cfg(target_os = "linux")]
pub use systemd::{DBUS_TIMEOUT, RealSystemd, SystemdManager, probe_systemd};
#[cfg(target_os = "linux")]
pub use worker::{EXECUTE_TIMEOUT, FilePin, WorkerContext, run_worker_mode};
