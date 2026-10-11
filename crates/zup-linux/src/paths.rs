use std::path::{Path, PathBuf};

use rustix::fs::{FileType, lstat};
use zup_artifact::{HostArchitecture, HostExecution, HostVersion, PlatformOs};
use zup_core::SelectedScope;
use zup_platform::SourceFilePolicy;

use crate::error::PathError;

pub fn native_architecture() -> Result<HostArchitecture, PathError> {
    let reported = uname().machine().to_string_lossy().into_owned();

    HostArchitecture::from_name(&reported).ok_or(PathError::UnknownArchitecture(reported))
}

pub fn host_version() -> Option<HostVersion> {
    leading_version(&uname().release().to_string_lossy())
}

fn leading_version(release: &str) -> Option<HostVersion> {
    let mut numbers = release
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .take(2)
        .filter_map(|part| part.parse::<u32>().ok());
    Some(HostVersion::new(
        numbers.next()?,
        numbers.next().unwrap_or(0),
        0,
    ))
}

pub fn additional_architectures(native: HostArchitecture) -> Vec<HostArchitecture> {
    match native {
        HostArchitecture::X86_64 => vec![HostArchitecture::X86],
        HostArchitecture::Arm64 => vec![HostArchitecture::Arm],
        HostArchitecture::X86 | HostArchitecture::Arm => Vec::new(),
    }
}

pub fn host_execution() -> HostExecution {
    let native = native_architecture().unwrap_or(HostArchitecture::X86_64);
    HostExecution {
        os: PlatformOs::Linux,
        native,
        emulated: additional_architectures(native),
        version: host_version(),
    }
}

fn uname() -> rustix::system::Uname {
    rustix::system::uname()
}

const STATE_FOLDER: &str = "zup";

const MACHINE_STATE_BASE: &str = "/var/lib";

pub fn state_root(scope: SelectedScope) -> Result<PathBuf, PathError> {
    match scope {
        SelectedScope::User => user_state_root(),
        SelectedScope::Machine => machine_state_root(),
    }
}

pub fn user_state_root() -> Result<PathBuf, PathError> {
    create_user_root(xdg::BaseDirectories::new().get_state_home())
}

fn create_user_root(base: Option<PathBuf>) -> Result<PathBuf, PathError> {
    let root = base.ok_or(PathError::NoStateHome)?.join(STATE_FOLDER);
    std::fs::create_dir_all(&root).map_err(|source| io_error(&root, source))?;
    canonical(&root)
}

pub fn machine_state_root() -> Result<PathBuf, PathError> {
    Ok(Path::new(MACHINE_STATE_BASE).join(STATE_FOLDER))
}

fn canonical(path: &Path) -> Result<PathBuf, PathError> {
    path.canonicalize().map_err(|source| io_error(path, source))
}

fn io_error(path: &Path, source: std::io::Error) -> PathError {
    PathError::io(path, source)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceEntryKind {
    Regular,

    Directory,

    SymbolicLink,

    Special,
}

impl SourceEntryKind {
    pub const fn is_readable_file(self) -> bool {
        matches!(self, Self::Regular)
    }

    fn of(stat: &rustix::fs::Stat) -> Self {
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::RegularFile => Self::Regular,
            FileType::Directory => Self::Directory,
            FileType::Symlink => Self::SymbolicLink,
            FileType::Fifo
            | FileType::Socket
            | FileType::BlockDevice
            | FileType::CharacterDevice => Self::Special,
            _ => Self::Special,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct LinuxSourceFilePolicy;

impl LinuxSourceFilePolicy {
    pub fn classify(path: &Path) -> std::io::Result<SourceEntryKind> {
        lstat(path)
            .map(|stat| SourceEntryKind::of(&stat))
            .map_err(std::io::Error::from)
    }
}

impl SourceFilePolicy for LinuxSourceFilePolicy {
    fn is_link(&self, path: &Path) -> Result<bool, std::io::Error> {
        match Self::classify(path) {
            Ok(kind) => Ok(kind == SourceEntryKind::SymbolicLink),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_host_reports_an_architecture_zup_models() {
        let native = native_architecture().expect("the kernel names this machine");
        assert!(matches!(
            native,
            HostArchitecture::X86
                | HostArchitecture::X86_64
                | HostArchitecture::Arm
                | HostArchitecture::Arm64
        ));
        assert!(
            !additional_architectures(native).contains(&native),
            "{native} cannot execute itself *additional* to itself"
        );
        assert_eq!(host_execution().os, PlatformOs::Linux);
    }

    #[rstest::rstest]
    #[case::vendor_suffix("6.8.0-45-generic", Some(HostVersion::new(6, 8, 0)))]
    #[case::azure_build("6.8.0-1028-azure", Some(HostVersion::new(6, 8, 0)))]
    #[case::two_components("5.15", Some(HostVersion::new(5, 15, 0)))]
    #[case::no_patch("6.1", Some(HostVersion::new(6, 1, 0)))]
    #[case::nothing_parsable("unknown", None)]
    fn a_version_reads_two_leading_numbers_and_ignores_the_rest(
        #[case] release: &str,
        #[case] expected: Option<HostVersion>,
    ) {
        assert_eq!(leading_version(release), expected, "{release}");
    }

    #[test]
    fn an_unmodelled_machine_is_refused_rather_than_guessed() {
        assert_eq!(
            HostArchitecture::from_name("x86_64"),
            Some(HostArchitecture::X86_64)
        );
        assert_eq!(
            HostArchitecture::from_name("aarch64"),
            Some(HostArchitecture::Arm64)
        );
        assert_eq!(HostArchitecture::from_name("riscv64"), None);
        assert_eq!(
            native_architecture().is_ok(),
            HostArchitecture::from_name(&native_architecture_name()).is_some(),
            "the two answers cannot disagree about the same kernel"
        );
    }

    fn native_architecture_name() -> String {
        uname().machine().to_string_lossy().into_owned()
    }

    #[test]
    fn the_user_root_is_created_and_the_machine_root_is_not() {
        let user = user_state_root().expect("a user state root");
        assert!(user.is_dir(), "{} should have been created", user.display());
        assert!(
            user.ends_with(STATE_FOLDER),
            "{} should end in the state folder",
            user.display()
        );

        let machine = machine_state_root().expect("a machine state root");
        assert_eq!(machine, Path::new("/var/lib/zup"));
        assert!(
            !machine.exists(),
            "creating the machine root is a privileged act, and this process has not been \
             given one"
        );
    }

    #[test]
    fn a_scope_selects_between_the_two_answers() {
        assert_eq!(
            state_root(SelectedScope::User).expect("user state"),
            user_state_root().expect("user state")
        );
        assert_eq!(
            state_root(SelectedScope::Machine).expect("machine state"),
            machine_state_root().expect("machine state")
        );
    }

    #[test]
    fn a_host_with_no_state_home_has_no_state_root() {
        assert!(matches!(
            create_user_root(None),
            Err(PathError::NoStateHome)
        ));
    }

    #[test]
    fn a_resolved_state_home_produces_a_created_canonical_root() {
        let base = tempfile::tempdir().expect("a temp base");
        let root = create_user_root(Some(base.path().to_path_buf())).expect("a user root");
        assert!(root.is_dir(), "{} should have been created", root.display());
        assert_eq!(
            root.file_name().and_then(|name| name.to_str()),
            Some(STATE_FOLDER)
        );
        assert_eq!(
            root,
            root.canonicalize().expect("canonical"),
            "and a state root is canonical, so one directory has one identity"
        );
    }

    fn classify(path: &Path) -> SourceEntryKind {
        LinuxSourceFilePolicy::classify(path).expect("classify")
    }

    #[test]
    fn the_four_kinds_are_told_apart() {
        let root = tempfile::tempdir().expect("a temp directory");
        let regular = root.path().join("payload.bin");
        std::fs::write(&regular, b"content").expect("write");
        assert_eq!(classify(&regular), SourceEntryKind::Regular);
        assert_eq!(classify(root.path()), SourceEntryKind::Directory);

        let link = root.path().join("link");
        std::os::unix::fs::symlink(&regular, &link).expect("symlink");
        assert_eq!(classify(&link), SourceEntryKind::SymbolicLink);
        assert!(
            LinuxSourceFilePolicy.is_link(&link).expect("is_link"),
            "and a link is a link to the portable trait's question as well"
        );

        let fifo = root.path().join("pipe");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo runs");
        if made.success() {
            assert_eq!(
                classify(&fifo),
                SourceEntryKind::Special,
                "a FIFO is not a file a build may read"
            );
            assert!(
                !LinuxSourceFilePolicy.is_link(&fifo).expect("is_link"),
                "and it is not a link either, which is exactly why a boolean is the wrong \
                 answer here"
            );
        }
    }

    #[test]
    fn a_link_is_classified_as_itself_and_not_as_its_destination() {
        let outside = tempfile::tempdir().expect("an outside directory");
        let secret = outside.path().join("secret");
        std::fs::write(&secret, b"not the project's bytes").expect("write");

        let root = tempfile::tempdir().expect("a temp directory");
        let link = root.path().join("payload.bin");
        std::os::unix::fs::symlink(&secret, &link).expect("symlink");

        assert_eq!(classify(&link), SourceEntryKind::SymbolicLink);
        assert!(
            !SourceEntryKind::SymbolicLink.is_readable_file(),
            "and it is not readable content"
        );
        assert!(
            LinuxSourceFilePolicy.is_link(&link).expect("is_link"),
            "so the portable trait's question is answered by the link itself"
        );
    }

    #[test]
    fn an_absent_path_is_neither_a_link_nor_an_error() {
        let root = tempfile::tempdir().expect("a temp directory");
        let absent = root.path().join("not-there");
        assert!(
            !LinuxSourceFilePolicy
                .is_link(&absent)
                .expect("an absent path is not a link"),
            "an absent source is missing, not a link, and not a failure"
        );
    }

    #[test]
    fn only_a_regular_file_is_readable_content() {
        assert!(SourceEntryKind::Regular.is_readable_file());
        assert!(!SourceEntryKind::Directory.is_readable_file());
        assert!(!SourceEntryKind::SymbolicLink.is_readable_file());
        assert!(!SourceEntryKind::Special.is_readable_file());
    }
}
