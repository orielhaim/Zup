#![deny(unsafe_code)]

//! Linux lifecycle backend and installer package reader.

#[cfg(target_os = "linux")]
mod backend;
mod capabilities;
mod carrier;
#[cfg(target_os = "linux")]
mod cleanup;
#[cfg(target_os = "linux")]
mod collection;
#[cfg(target_os = "linux")]
mod data_access;
#[cfg(target_os = "linux")]
mod elevate;
mod error;
#[cfg(target_os = "linux")]
mod executor;
#[cfg(target_os = "linux")]
mod fs;
#[cfg(target_os = "linux")]
mod generation;
#[cfg(target_os = "linux")]
mod history;
#[cfg(target_os = "linux")]
mod input;
mod integration;
#[cfg(target_os = "linux")]
mod ledger;
mod lifecycle;
mod locations;
mod lowering;
#[cfg(target_os = "linux")]
mod machine;
#[cfg(target_os = "linux")]
mod native_service;
mod openrc;
#[cfg(target_os = "linux")]
mod openrc_native;
#[cfg(target_os = "linux")]
mod operation_host;
#[cfg(target_os = "linux")]
mod package_process;
#[cfg(target_os = "linux")]
mod paths;
#[cfg(target_os = "linux")]
mod persistent;
#[cfg(target_os = "linux")]
mod pkexec;
#[cfg(target_os = "linux")]
mod prerequisites;
#[cfg(target_os = "linux")]
mod refresh;
mod resolve;
#[cfg(target_os = "linux")]
mod run;
#[cfg(target_os = "linux")]
mod service_control;
#[cfg(target_os = "linux")]
mod service_exec;
#[cfg(target_os = "linux")]
mod service_host;
#[cfg(target_os = "linux")]
mod service_ops;
mod services;
#[cfg(target_os = "linux")]
mod session_host;
#[cfg(target_os = "linux")]
mod socket;
#[cfg(target_os = "linux")]
mod state;
#[cfg(target_os = "linux")]
mod systemd;
#[cfg(target_os = "linux")]
mod systemd_policy;
#[cfg(all(target_os = "linux", feature = "test-support"))]
#[allow(unsafe_code)]
pub mod test_support;
#[cfg(target_os = "linux")]
mod worker;

#[cfg(target_os = "linux")]
pub use backend::{ExecutionReport, LinuxRuntimeBackend};
pub use capabilities::{
    is_supported_linux_target, linux_capability_errors, linux_selection_errors,
    operation_capability_errors,
};
pub use carrier::Carrier;
pub use error::{ExecError, IpcError, PathError, PlanError};
#[cfg(target_os = "linux")]
pub use executor::{FileIntent, FileOperation, LinuxFileExecutor, ServiceSupport, snapshot_target};
#[cfg(target_os = "linux")]
pub use fs::{
    ChildStat, EXECUTABLE_PAYLOAD_MODE, EntryKind, MACHINE_INSTALL_DIRECTORY_MODE, OwnedDirectory,
    PAYLOAD_FILE_MODE, STATE_DIRECTORY_MODE, STATE_FILE_MODE, StreamedRegular,
    USER_INSTALL_DIRECTORY_MODE, anchor_reachable, create_parents, ensure_parent, open_parent,
    sync_directory, verify_ancestor_trust,
};
#[cfg(target_os = "linux")]
pub use input::{
    ServiceCompilation, compile_execution_plan, compile_machine_execution_plan,
    ledger_has_services, requires_service_manager, snapshot_services,
};
#[cfg(target_os = "linux")]
pub use integration::{generated_for_plan, phase_keeps_generated, reap_generated, stage_generated};
#[cfg(target_os = "linux")]
pub use ledger::LinuxInstallationStore;
pub use lifecycle::LinuxOutcome;
pub use locations::{
    LinuxInstallLocationResolver, MACHINE_PROGRAMS_ROOT, MACHINE_SHARED_DATA_ROOT,
    MACHINE_STATE_ROOT, MachineRoots, user_data_home, user_data_home_in, user_programs_root,
    user_programs_root_in,
};
pub use lowering::{linux_target_path, target_path_from_host, to_host_path};
#[cfg(target_os = "linux")]
pub use machine::{
    MACHINE_LOCK_FILE_MODE, MACHINE_PRIVATE_DIR_MODE, MACHINE_PRIVATE_FILE_MODE,
    MACHINE_PUBLIC_FILE_MODE, MACHINE_STATE_DIR_MODE, MachineDestination, MachineStateReadiness,
    SYSTEMD_UNIT_FILE_MODE, SystemdRoots, assess_machine_state_root, authorize_machine_destination,
    authorize_machine_install_directory, authorize_systemd_unit, ensure_machine_state_root,
    normalize_state_modes, resolve_machine_state_root, verify_ledger_trust,
    verify_machine_hierarchy, verify_machine_install_roots, verify_machine_structure,
    verify_trusted_state_file,
};
#[cfg(target_os = "linux")]
pub use paths::{
    LinuxSourceFilePolicy, SourceEntryKind, machine_state_root, state_root, user_state_root,
};
#[cfg(target_os = "linux")]
pub use pkexec::{LaunchOutcome, PkexecLauncher, SystemPkexec, WorkerChild, map_launch};
pub use resolve::{resolve_target, validate_target_plan};
#[cfg(target_os = "linux")]
pub use run::{LinuxRunRequest, run, run_outcome};
#[cfg(target_os = "linux")]
pub use service_ops::{
    MAX_UNIT_BYTES, admin_override_dir, check_collisions, check_no_full_override, desired_policy,
    load_path_dirs, policy_for_state, read_canonical_source, refuse_source_symlink,
    validate_changes, validate_executable, validate_executable_live,
};
pub use services::{
    MINIMUM_SYSTEMD_VERSION, SERVICE_TYPE, SYSTEMD_UNIT_DIR, SystemdUnit, WANTED_BY,
    parse_manager_version,
};
#[cfg(target_os = "linux")]
pub use session_host::LinuxSessionHost;
#[cfg(target_os = "linux")]
pub use socket::{
    FRAME_TIMEOUT, HANDSHAKE_TIMEOUT, PeerIdentity, PeerPin, Rendezvous, peer_alive, peer_identity,
    pin_peer,
};
#[cfg(target_os = "linux")]
pub use systemd::{DBUS_TIMEOUT, RealSystemd, SystemdManager, probe_systemd};
#[cfg(target_os = "linux")]
pub use worker::{EXECUTE_TIMEOUT, FilePin, WorkerContext, run_worker_mode};

#[cfg(all(target_os = "linux", feature = "online"))]
mod update;

#[cfg(all(test, target_os = "linux"))]
mod requirements_fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/deployment.rs"
    ));
}

#[cfg(target_os = "linux")]
pub use persistent::resolve_persistent_bindings;
