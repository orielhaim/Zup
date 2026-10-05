//! What a Linux build is allowed to read out of a project's source tree.
//!
//! A prerequisite read through a link is a read of bytes the project never
//! declared, so materialization refuses sources whose ancestry contains one. That
//! question - "is this entry a link?" - is a property of the host filesystem, and
//! Linux's answer is broader than `std::fs`'s in a way that matters.
//!
//! [`std::fs::symlink_metadata`] reports a symbolic link as a link, and it
//! reports a directory or a file as itself. That is a complete answer for
//! `std::fs`'s own question, and an incomplete one for this one: a Linux
//! filesystem also has *special* files - a FIFO that blocks on open, a device
//! node that talks to hardware, a socket - and `std::fs` reports every one of them
//! as an ordinary file. Reading one is not a read of declared content. So this
//! module does not reduce the question to a boolean; it classifies an entry into
//! the four kinds a Linux directory entry can have, and lets the caller refuse
//! the ones that are not regular files.
//!
//! It also does not reuse [`PortableSourceFilePolicy`] merely because that one is
//! close enough to compile. The two answers differ on precisely the case that
//! matters: `PortableSourceFilePolicy` says a FIFO is not a link and would let a
//! materializer read it, and this one names it as a special file so the caller can
//! refuse it. Weakening the Linux rule to match the portable one would make the
//! portable one the ceiling, which is the wrong direction for a security rule.
//!
//! # What the race resistance here actually is
//!
//! Classification reads the entry *itself* rather than what it resolves to, so a
//! link is classified as a link. That is what `lstat` is for, and `std::fs`'s
//! `symlink_metadata` asks the same question; the reason this module is not a
//! one-line wrapper around the portable policy is the file-type classification
//! above, not the syscall.
//!
//! What it does *not* claim is that a read which follows the classification is
//! safe against a hostile concurrent writer. Closing that gap is a
//! descriptor-relative open-and-verify, which belongs to the materializer that
//! owns the open, not to a policy that only answers questions. What this claims is
//! narrower and is the whole of what a source-inspection policy is for: a caller
//! can refuse a special file *before* it opens one, and cannot be misled by a
//! link into inspecting bytes the project never declared.

use std::path::Path;

use rustix::fs::{FileType, lstat};
use zup_platform::SourceFilePolicy;

/// What a directory entry is, in the four kinds a Linux filesystem has.
///
/// A closed set rather than a boolean, because "is it a link" is not the question
/// this has to answer. A FIFO is not a link and is still not a file a build may
/// read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceEntryKind {
    /// An ordinary file. The only kind a build may read.
    Regular,
    /// A directory. Traversable, and nothing inside it is implied.
    Directory,
    /// A symbolic link. Never followed by source inspection.
    SymbolicLink,
    /// Anything else: a FIFO, a socket, a block or character device.
    Special,
}

impl SourceEntryKind {
    /// Whether a build may read this entry's bytes.
    ///
    /// A directory is not a source file either, but it is a thing materialization
    /// walks into rather than a thing it reads, so it is not what this decides.
    pub const fn is_readable_file(self) -> bool {
        matches!(self, Self::Regular)
    }

    /// The kind a `lstat` mode names.
    ///
    /// `rustix` splits the file-type bits out of a mode; this exists because its
    /// `FileType` distinguishes three kinds of special file and a caller only ever
    /// wants one answer about them. The unmatched arm is not dead code: the enum
    /// is `#[non_exhaustive]` because the kernel may add a type, and the honest
    /// reading of an unknown one is "not a file this build may read".
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

/// The source policy a Linux build injects.
#[derive(Debug, Default, Clone, Copy)]
pub struct LinuxSourceFilePolicy;

impl LinuxSourceFilePolicy {
    /// Classify `path` as itself, never as what it points at.
    ///
    /// A path that does not exist is an error here, unlike the answer
    /// [`SourceFilePolicy`] gives for a link. The two questions are different: "is
    /// this a link" has to answer about a path that may not be there, because a
    /// materializer asking that question has usually just found the path absent
    /// and wants to know whether that absence is a link or a plain gap.
    /// Classification is only ever asked of a path that exists.
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

    fn classify(path: &Path) -> SourceEntryKind {
        LinuxSourceFilePolicy::classify(path).expect("classify")
    }

    /// The four kinds have to be told apart, because that is the whole reason this
    /// exists rather than being `PortableSourceFilePolicy`. A FIFO is the case that
    /// matters: it is not a link, and reading one blocks forever.
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

    /// A link is classified as itself, never as its destination. A policy that
    /// resolved a link would report the *target's* kind, and a link to a regular
    /// file would read as readable - which is the substitution this refuses.
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

    /// The portable policy's contract has to hold here too, because it is what a
    /// caller with no host adapter gets: a path that does not exist is not a link,
    /// and it is not an error either. Getting this wrong turns a missing source
    /// into a build failure that names the wrong thing.
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

    /// Only a regular file is readable content. Every other kind is something a
    /// build must refuse rather than read, which is the answer that a boolean
    /// cannot express.
    #[test]
    fn only_a_regular_file_is_readable_content() {
        assert!(SourceEntryKind::Regular.is_readable_file());
        assert!(!SourceEntryKind::Directory.is_readable_file());
        assert!(!SourceEntryKind::SymbolicLink.is_readable_file());
        assert!(!SourceEntryKind::Special.is_readable_file());
    }
}
