//! Windows platform backend for zup.
//!
//! Prefers focused `windows-*` crates over the `windows` umbrella crate.
//! Generated Win32 bindings stay private to this crate.

mod bindings;
mod bootstrap_fs;
mod bundle_packager;
mod cmdline;
mod durable;
mod file_executor;
mod fs_bindings;
mod inspect;
mod integration;
mod known_folders;
mod ledger;
mod lowering;
mod payload_overlay;
mod pipe;
mod planning;
mod prerequisites;
mod registry;
mod resolve;
mod restart_manager;
mod runtime;
mod scm;
mod search_path;
mod services;
mod shell_link;
mod shortcut_name;
mod shortcuts;
mod source_policy;
mod transaction_payload;
mod transport;
mod transport_bindings;
mod worker;
mod worker_rt;

pub use bootstrap_fs::{WindowsBootstrapFileSystem, windows_bootstrap_file_system};
pub use bundle_packager::{
    AutoPayloadSource, BundleError, EmbeddedBundle, EmbeddedPayloadSource, OverlayPayloadSource,
    PeSubsystem, build_self_contained_executable, embed_bundle_file, read_pe_frontend,
    read_pe_subsystem, read_pe_target, validate_embedded_bundle_target, validate_pe_frontend,
};
pub use cmdline::{
    command_spec, command_spec_from_command_line, commands_match, format_command_line,
    parse_command_line, quote_arg, split_command_line,
};
pub use durable::{
    DurableError, InstallationLock, copy_new_durable, create_durable, move_durable, volume_root,
    write_durable,
};
pub use file_executor::{
    CreateFileReceipt, FileProgress, NullProgress, OperationReceipt, ProgressSink,
    ReplaceFileReceipt, StageFileReceipt, WindowsFileExecutor, WindowsFileExecutorError,
    apply_node, reconcile_node, transaction_receipt, verify_installed_file,
};
pub use inspect::{InspectError, inspect_files, inspect_target, inspect_target_with};
pub use integration::{
    IntegrationError, apply_managed, apply_owned_removal, inspect_uninstall_registration,
    notify_committed_path_change, reconcile_managed, reconcile_owned_removal, rollback_managed,
};
pub use known_folders::WindowsInstallLocationResolver;
pub use ledger::{InstallLedgerStore, LedgerError};
pub use lowering::{
    TargetPathLoweringError, TargetPathValidationError, to_host_path, validate_windows_target_path,
    windows_target_path_identity,
};
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
pub use prerequisites::{
    WindowsPrerequisiteDetector, WindowsPrerequisiteProvider, package_requirements,
    runtime_requirements,
};
pub use registry::{RegistryError, RegistryReader, RegistryValue, WindowsRegistryReader};
pub use resolve::{TargetResolveError, WindowsTargetContext, resolve_target};
pub use restart_manager::{
    BlockingProcess, FilePreflight, blocked_reason, mutating_paths, plan_mutating_paths, preflight,
};
pub use runtime::{
    OverlayPolicy, WindowsRuntimeBackend, run_install, run_install_control,
    run_install_control_with_policy, run_local_install,
};
pub use search_path::{
    PATH_VALUE_NAME, VALUE_TYPE_EXPAND, VALUE_TYPE_MISSING, VALUE_TYPE_PLAIN,
    contains as search_path_contains, lost_expansion, split as split_search_path, write_value_type,
};
pub use services::{FakeServiceReader, ServiceReader, WindowsServiceReader};
pub use shortcuts::{FakeShortcutReader, ShortcutReader, WindowsShortcutReader};
pub use source_policy::WindowsSourceFilePolicy;
pub use transaction_payload::{
    AppsFeaturesOperation, AppsFeaturesState, AppsFeaturesValue, TransactionPayloadError,
};
#[cfg(feature = "test-launcher")]
pub use transport::launch_worker_for_test;
pub use transport::{
    ProcessHandle, TransportError, UserSid, is_process_elevated, launch_elevated_worker, pipe_name,
    pipe_path, verify_client_pid, verify_server_pid, wait_for_process_exit,
};
pub use worker::{
    WorkerBootstrap, WorkerError, WorkerSession, current_exe, decode_frame, encode_reply,
    format_bootstrap, parse_bootstrap, plan_hash_hex, worker_capabilities,
};
pub use worker_rt::run_worker;
#[cfg(feature = "test-launcher")]
pub use worker_rt::run_worker_for_test;
pub use zup_transaction::FilePrecondition;
