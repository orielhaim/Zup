//! Windows platform backend for zup.
//!
//! Prefers focused `windows-*` crates over the `windows` umbrella crate.
//! Generated Win32 bindings stay private to this crate.

mod bindings;
mod cmdline;
mod durable;
mod file_executor;
mod fs_bindings;
mod inspect;
mod integration;
mod known_folders;
mod ledger;
mod payload_overlay;
mod pipe;
mod planning;
mod prerequisites;
mod registry;
mod resolve;
mod restart_manager;
mod scm;
mod services;
mod shell_link;
mod shortcut_name;
mod shortcuts;
mod transport;
mod transport_bindings;
mod worker;
mod worker_rt;

pub use cmdline::{
    command_spec, command_spec_from_command_line, commands_match, format_command_line,
    parse_command_line, path_entry_matches, quote_arg, split_command_line,
};
pub use durable::{
    DurableError, InstallationLock, copy_new_durable, create_durable, move_durable, volume_root,
    write_durable,
};
pub use file_executor::{
    CreateFileReceipt, FileProgress, NullProgress, OperationReceipt, ProgressSink,
    ReplaceFileReceipt, StageFileReceipt, WindowsFileExecutor, WindowsFileExecutorError,
    apply_node, reconcile_node,
};
pub use inspect::{InspectError, inspect_files, inspect_target, inspect_target_with};
pub use integration::{
    IntegrationError, apply_managed, apply_owned_removal, inspect_uninstall_registration,
    notify_committed_path_change, reconcile_managed, reconcile_owned_removal, rollback_managed,
};
pub use known_folders::WindowsKnownFolderResolver;
pub use ledger::{InstallLedgerStore, LedgerError};
pub use payload_overlay::{
    PAYLOAD_OVERLAY_DIRECTORY, PayloadOverlayError, PayloadOverlayFileIdentity,
    PayloadOverlayIdentity, cleanup_app_payload_overlays, cleanup_payload_overlay,
    is_plugin_payload_path, materialize_payload_overlay, payload_overlay_base_root,
    validate_payload_overlay_base, verify_payload_overlay,
};
pub use pipe::{
    ClientReader, ClientWriter, HELLO_TIMEOUT, PipeError, PipeSecurity, PipeServer, ServerReader,
    ServerWriter, WORKER_CONNECT_TIMEOUT, check_version, frame_client, frame_server,
};
pub use planning::{WindowsPlanError, plan_target_lifecycle, plan_target_lifecycle_with_frontend};
pub use prerequisites::{WindowsPrerequisiteDetector, WindowsPrerequisiteProvider};
pub use registry::{
    RegistryError, RegistryReader, RegistryValue, WindowsRegistryReader, read_path_value,
    split_path_value,
};
pub use resolve::{TargetResolveError, WindowsTargetContext, resolve_target};
pub use restart_manager::{BlockingProcess, FilePreflight, mutating_paths, preflight};
pub use services::{FakeServiceReader, ServiceReader, WindowsServiceReader};
pub use shortcuts::{FakeShortcutReader, ShortcutReader, WindowsShortcutReader};
#[cfg(feature = "test-launcher")]
pub use transport::launch_worker_for_test;
pub use transport::{
    ProcessHandle, TransportError, UserSid, is_process_elevated, launch_elevated_worker, pipe_name,
    pipe_path, verify_client_pid, verify_server_pid, wait_for_process_exit,
};
pub use worker::{
    WorkerBootstrap, WorkerError, WorkerSession, current_exe, decode_frame, encode_reply,
    format_bootstrap, parse_bootstrap, plan_hash_hex,
};
pub use worker_rt::run_worker;
#[cfg(feature = "test-launcher")]
pub use worker_rt::run_worker_for_test;
pub use zup_exec::FilePrecondition;
