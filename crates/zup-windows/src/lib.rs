//! Windows installation backend and Windows installer composition.
//!
//! Two responsibilities share this crate, and the build host decides which one
//! compiles:
//!
//! - **Composition** turns a runtime template and a portable package into an
//!   installer for a Windows target. It reads and writes PE files as bytes, so
//!   it runs on any host. These modules have no `cfg` gate.
//! - **The native runtime** installs, repairs, and removes software through
//!   Win32, the registry, services, and named pipes. It executes only on
//!   Windows, and every module it needs is gated on `cfg(windows)`.
//!
//! The build engine depends on the first and never on the second: bundles are
//! embedded with [`build_self_contained_executable`], universal artifacts with
//! [`compose_universal_executable`], and target plans are validated with
//! [`resolve_target`] against [`CanonicalWindowsLocations`]. Both embedders are
//! thin adapters over the portable [`zup_compose`] pipeline, which owns the
//! target-driven composition both hosts share. The live machine
//! resolver behind [`WindowsInstallLocationResolver`] stays a runtime detail.

#[cfg(windows)]
mod acquired;
#[cfg(windows)]
mod bindings;
#[cfg(windows)]
mod bootstrap_fs;
#[cfg(windows)]
mod bootstrap_plan;
mod bundle_packager;
mod cmdline;
#[cfg(windows)]
mod content_store;
#[cfg(windows)]
mod durable;
#[cfg(windows)]
mod file_executor;
#[cfg(windows)]
mod host;
#[cfg(windows)]
mod host_dirs;
#[cfg(windows)]
mod inspect;
#[cfg(windows)]
mod integration;
#[cfg(windows)]
mod ledger;
mod locations;
mod lowering;
#[cfg(windows)]
mod machine_authority;
#[cfg(windows)]
mod machine_fs;
#[cfg(windows)]
mod machine_state;
#[cfg(windows)]
mod operation_host;
#[cfg(windows)]
mod path_safety;
#[cfg(windows)]
mod payload_overlay;
mod pe_resources;
#[cfg(windows)]
mod pipe;
#[cfg(windows)]
mod planning;
#[cfg(windows)]
mod prerequisites;
#[cfg(windows)]
mod preset_stage;
#[cfg(windows)]
mod process;
#[cfg(windows)]
mod registry;
mod resolve;
#[cfg(windows)]
mod restart_manager;
#[cfg(windows)]
mod runtime;
#[cfg(windows)]
mod scm;
#[cfg(windows)]
mod search_path;
#[cfg(windows)]
mod services;
#[cfg(windows)]
mod shell_link;
mod shortcut_name;
#[cfg(windows)]
mod shortcuts;
#[cfg(windows)]
pub mod signing;
#[cfg(windows)]
mod source_policy;
#[cfg(windows)]
mod transaction_payload;
#[cfg(windows)]
mod transport;
mod universal;
#[cfg(windows)]
mod worker_rt;

#[cfg(windows)]
pub use acquired::{
    AcquiredBundle, AcquiredRelease, HandoffRejection, VerifiedHandoff, accept_handoff, frontend,
    open_cache, own_digest, verify, write_handoff,
};
#[cfg(windows)]
pub use bootstrap_fs::{WindowsBootstrapFileSystem, windows_bootstrap_file_system};
#[cfg(windows)]
pub use bootstrap_plan::{BootstrapPlanError, peer_user_state_root};
pub use bundle_packager::{
    AutoPayloadSource, BundleError, EmbeddedBundle, EmbeddedPayloadSource, OverlayPayloadSource,
    build_plan_only_executable, build_self_contained_executable, embed_bundle_file,
    is_executable_image, plan_only_runtime_bytes, read_frontend, sidecar_package_path,
    validate_embedded_bundle_target, validate_frontend,
};
pub use cmdline::{
    command_spec, command_spec_from_command_line, commands_match, format_command_line,
    parse_command_line, quote_arg, split_command_line,
};
#[cfg(windows)]
pub use content_store::{
    ContentStoreError, content_store_base, ensure_directory, preset_executable_name,
    validate_content_store_base, verify_directory_chain,
};
#[cfg(windows)]
pub use durable::{DurableError, copy_new_durable, move_durable, volume_root, write_durable};
#[cfg(windows)]
pub use file_executor::{
    CreateFileReceipt, FileProgress, NullProgress, OperationReceipt, ProgressSink,
    ReplaceFileReceipt, StageFileReceipt, WindowsFileExecutor, WindowsFileExecutorError,
    apply_node, reconcile_node, transaction_receipt, verify_installed_file,
};
#[cfg(windows)]
pub use host::{
    HostError, MachineSupport, NativeMachine, ProcessMachines, emulated_architectures,
    host_execution, host_version, machine_support, native_machine, process_machines,
};
#[cfg(windows)]
pub use host_dirs::{
    HostDirError, WindowsInstallLocationResolver, shared_data, user_data, user_desktop,
};
#[cfg(windows)]
pub use inspect::{InspectError, InspectReads, inspect_files, inspect_target, inspect_target_with};
#[cfg(windows)]
pub use integration::{
    IntegrationError, apply_managed, apply_owned_removal, inspect_uninstall_registration,
    notify_committed_path_change, reconcile_managed, reconcile_owned_removal, rollback_managed,
};
#[cfg(windows)]
pub use ledger::{InstallLedgerStore, LedgerError, ReleaseRecord};
pub use locations::CanonicalWindowsLocations;
pub use lowering::{
    TargetPathLoweringError, TargetPathValidationError, to_host_path, validate_windows_target_path,
    windows_target_path_identity,
};
#[cfg(windows)]
pub use machine_authority::MachineRoot;
#[cfg(windows)]
pub use machine_state::{
    MachineStateError, default_state_root, ensure_state_root, plain_path_text, resolve_state_root,
};
#[cfg(windows)]
pub use operation_host::{WindowsContent, WindowsOperationHost};
#[cfg(windows)]
pub use payload_overlay::{
    PAYLOAD_OVERLAY_DIRECTORY, PayloadOverlayError, PayloadOverlayFileIdentity,
    PayloadOverlayIdentity, cleanup_app_payload_overlays, cleanup_payload_overlay,
    is_plugin_payload_path, materialize_payload_overlay, payload_overlay_base_root,
    validate_payload_overlay_base, verify_payload_overlay,
};
pub use pe_resources::{ResourceError, apply_icon, read_resource, write_resources};
#[cfg(windows)]
pub use pipe::{
    ClientReader, ClientWriter, HANDSHAKE_TIMEOUT, PipeError, PipeSecurity, PipeServer,
    ServerReader, ServerWriter, WORKER_CONNECT_TIMEOUT, check_version, frame_client, frame_server,
};
#[cfg(windows)]
pub use planning::{WindowsPlanError, plan_target_lifecycle, plan_target_lifecycle_with_frontend};
#[cfg(windows)]
pub use prerequisites::{
    WindowsPrerequisiteDetector, WindowsPrerequisiteProvider, package_requirements,
    runtime_requirements,
};
#[cfg(windows)]
pub use preset_stage::{StagedPreset, stage_session};
#[cfg(windows)]
pub use process::{
    ChildProcess, HandOff, LaunchError, LaunchRequest, Launcher, WindowsLauncher, launch,
    quote_argument,
};
#[cfg(windows)]
pub use registry::{RegistryError, RegistryValue, open_classes_key, open_environment_key};
pub use resolve::{TargetResolveError, WindowsTargetContext, resolve_target};
#[cfg(windows)]
pub use restart_manager::{
    BlockingProcess, FilePreflight, blocked_reason, mutating_paths, plan_mutating_paths, preflight,
};
#[cfg(windows)]
pub use runtime::{OverlayPolicy, WindowsRuntimeBackend};
#[cfg(windows)]
pub use scm::ScmError;
#[cfg(windows)]
pub use search_path::{
    PATH_VALUE_NAME, VALUE_TYPE_EXPAND, VALUE_TYPE_MISSING, VALUE_TYPE_PLAIN,
    contains as search_path_contains, lost_expansion, split as split_search_path, write_value_type,
};
#[cfg(windows)]
pub use services::{ServiceError, read_service};
#[cfg(windows)]
pub use shell_link::ShellLinkError;
pub use shortcut_name::{ShortcutNameError, validate_shortcut_filename};
#[cfg(windows)]
pub use shortcuts::{ShortcutError, read_shortcut};
#[cfg(windows)]
pub use source_policy::WindowsSourceFilePolicy;
#[cfg(windows)]
pub use transaction_payload::{
    AppsFeaturesOperation, AppsFeaturesState, AppsFeaturesValue, TransactionPayloadError,
};
#[cfg(all(windows, feature = "test-launcher"))]
pub use transport::launch_worker_for_test;
#[cfg(windows)]
pub use transport::{
    ProcessHandle, TransportError, UserSid, is_process_elevated, launch_elevated_worker, pipe_name,
    pipe_path, verify_client_pid, verify_server_pid, wait_for_process_exit,
};
#[cfg(windows)]
pub use universal::stage_variant;
pub use universal::{
    PeSegments, Selection, StagedVariant, UniversalArtifact, UniversalError, UniversalLayout,
    compose_universal_executable, read_variant_manifest, staged_descriptor,
    verify_selected_variant,
};
#[cfg(all(windows, feature = "test-launcher"))]
pub use worker_rt::run_worker_for_test;
#[cfg(windows)]
pub use worker_rt::{
    WorkerBootstrap, WorkerError, WorkerSession, current_exe, decode_frame, encode_reply,
    format_bootstrap, parse_bootstrap, plan_hash_hex, run_worker, worker_capabilities,
};
