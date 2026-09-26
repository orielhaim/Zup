//! Typed build and materialization errors.

use std::io;
use std::path::PathBuf;

use miette::Diagnostic;
use thiserror::Error;

/// Errors produced while materializing installer sources into a build plan.
#[derive(Debug, Error, Diagnostic)]
pub enum BuildError {
    /// No target profile was selected for materialization.
    #[error("no target profiles were selected")]
    #[diagnostic(code(zup_build::empty_target_selection))]
    EmptyTargetSelection,

    /// A target profile was selected more than once.
    #[error("target profile `{profile}` was selected more than once")]
    #[diagnostic(code(zup_build::duplicate_target_profile))]
    DuplicateTargetProfile { profile: String },

    /// Two selected profiles resolve to the same canonical target.
    #[error("canonical target `{target}` was selected more than once")]
    #[diagnostic(code(zup_build::duplicate_target))]
    DuplicateTarget { target: String },

    /// A target config and compiled installer do not describe the same target.
    #[error(
        "target profile `{profile}` is configured for `{config}`, but its installer targets `{installer}`"
    )]
    #[diagnostic(code(zup_build::target_mismatch))]
    TargetMismatch {
        profile: String,
        config: String,
        installer: String,
    },

    /// A target-specific materialization operation failed.
    #[error("target profile `{profile}`: {source}")]
    #[diagnostic(code(zup_build::target))]
    Target {
        profile: String,
        #[source]
        source: Box<BuildError>,
    },

    /// The configured source root does not exist.
    #[error("source directory `{path}` does not exist")]
    #[diagnostic(code(zup_build::source_missing))]
    SourceMissing {
        path: PathBuf,
        #[source_code]
        src: Option<miette::NamedSource<String>>,
    },

    /// The source root exists but is not a directory.
    #[error("source path `{path}` is not a directory")]
    #[diagnostic(code(zup_build::source_not_directory))]
    SourceNotDirectory { path: PathBuf },

    /// The source root leaves the project directory.
    #[error("source directory `{path}` escapes the project root")]
    #[diagnostic(
        code(zup_build::source_escapes_project),
        help(
            "`[build.targets.<profile>].source.directory` is resolved relative to `zup.toml` and must stay inside the project"
        )
    )]
    SourceEscapesProject { path: PathBuf },

    /// A `[[files]]` pattern is not a valid glob.
    #[error("invalid glob pattern `{pattern}`: {message}")]
    #[diagnostic(code(zup_build::invalid_glob))]
    InvalidGlob { pattern: String, message: String },

    /// A pattern matched zero files and `allow_empty` is false.
    #[error("file pattern `{pattern}` matched no files")]
    #[diagnostic(
        code(zup_build::pattern_matched_nothing),
        help("set `allow_empty = true` on this [[files]] entry if empty matches are intentional")
    )]
    PatternMatchedNothing { pattern: String },

    /// A discovered path cannot be represented as a portable relative path.
    #[error("path `{path}` cannot be represented as a portable relative path: {reason}")]
    #[diagnostic(code(zup_build::path_not_representable))]
    PathNotRepresentable { path: String, reason: String },

    /// A relative path is unsafe (`..`, absolute, empty, …).
    #[error("unsafe relative path `{path}`: {reason}")]
    #[diagnostic(code(zup_build::unsafe_relative_path))]
    UnsafeRelativePath { path: String, reason: String },

    /// A `[[files]]` match is a symlink.
    #[error("pattern `{pattern}` matched symlink `{path}`; symlinks are not packaged")]
    #[diagnostic(
        code(zup_build::matched_symlink),
        help("package real files only; symlink install semantics will be a future resource type")
    )]
    MatchedSymlink { path: PathBuf, pattern: String },

    /// A `[[files]]` match is a special filesystem object.
    #[error("pattern `{pattern}` matched special file `{path}`")]
    #[diagnostic(code(zup_build::matched_special_file))]
    MatchedSpecialFile { path: PathBuf, pattern: String },

    /// Two resolved files target the same logical destination exactly.
    #[error("destination collision at `{destination}` ({first} and {second})")]
    #[diagnostic(code(zup_build::destination_collision))]
    DestinationCollision {
        destination: String,
        first: String,
        second: String,
    },

    /// Reading a source file failed.
    #[error("failed to read source file `{path}`")]
    #[diagnostic(code(zup_build::source_read_failure))]
    SourceReadFailure {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// File metadata changed while the file was being hashed.
    #[error("source file `{path}` changed while being hashed")]
    #[diagnostic(
        code(zup_build::source_changed_during_build),
        help("re-run the build after the source tree is stable")
    )]
    SourceChangedDuringBuild { path: PathBuf },

    /// Summing payload sizes overflowed `u64`.
    #[error("payload size overflow")]
    #[diagnostic(code(zup_build::size_overflow))]
    SizeOverflow,

    #[error("trusted update root exceeds the 1 MiB build limit")]
    #[diagnostic(code(zup_build::update_root_too_large))]
    UpdateRootTooLarge,

    #[error("plugin source `{path}` does not exist")]
    #[diagnostic(code(zup_build::plugin_source_missing))]
    PluginSourceMissing { path: PathBuf },

    #[error("plugin source `{path}` escapes the project root")]
    #[diagnostic(code(zup_build::plugin_source_escapes_project))]
    PluginSourceEscapesProject { path: PathBuf },

    #[error("plugin source `{path}` is a symlink")]
    #[diagnostic(code(zup_build::plugin_source_symlink))]
    PluginSourceSymlink { path: PathBuf },

    #[error("plugin source `{path}` is not a regular file")]
    #[diagnostic(code(zup_build::plugin_source_not_regular))]
    PluginSourceNotRegular { path: PathBuf },

    #[error("plugin source `{path}` is {size} bytes; the limit is {limit} bytes")]
    #[diagnostic(code(zup_build::plugin_source_too_large))]
    PluginSourceTooLarge {
        path: PathBuf,
        size: u64,
        limit: u64,
    },

    #[error("plugin source `{path}` changed while being hashed")]
    #[diagnostic(code(zup_build::plugin_source_changed))]
    PluginSourceChanged { path: PathBuf },

    #[error("manifest declares {count} plugins; the limit is {limit}")]
    #[diagnostic(code(zup_build::too_many_plugin_declarations))]
    TooManyPluginDeclarations { count: usize, limit: usize },

    #[error("plugin `{id}` has no matching resolved source")]
    #[diagnostic(code(zup_build::plugin_source_mismatch))]
    PluginSourceMismatch { id: String },

    #[error("embedded prerequisite `{id}` is missing or unsafe")]
    #[diagnostic(code(zup_build::prerequisite_source))]
    PrerequisiteSource { id: String, path: PathBuf },

    #[error("embedded prerequisite `{id}` does not match its declared size or digest")]
    #[diagnostic(code(zup_build::prerequisite_identity))]
    PrerequisiteIdentity { id: String, path: PathBuf },

    /// An I/O error occurred while resolving the source root.
    #[error("I/O error at `{path}`")]
    #[diagnostic(code(zup_build::io))]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}
