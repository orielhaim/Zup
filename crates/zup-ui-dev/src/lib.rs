//! A development environment for a preset author.
//!
//! Everything here is a host. The preset is a child process reached over the
//! production transport, speaking the production protocol, running the production
//! SDK, and unable to tell that the machine on the other end is simulated. That
//! is the claim the whole thing exists to make true, and it is why the state
//! machine is shared with an installer rather than written again: two
//! implementations would drift, and a preset developed against the drifted one
//! would meet the other.
//!
//! This crate is the *source* half, and only that. The machine, the child, the
//! session, the controls and the watcher are `zup-preview`'s, because `zup
//! preview` presents the same window for a different author and a preview
//! environment per command is a preview environment per command that will be
//! different next year. What is left here is what only a preset author needs: a
//! Cargo project, a compiler to supervise, and the rule that a save to Rust is a
//! compile while a save to a document is not.

#![deny(unsafe_code)]

mod build;
mod development;
mod project;
mod session;
mod watch;

pub use build::{Build, Diagnostic, Supervisor};
pub use development::{Development, DevelopmentError, FILE_NAME};
pub use project::{Project, ProjectError};
pub use session::{Request, Session, SessionError};
pub use watch::{Change, Watched, Watcher};

/// Run a development session in `root` until somebody quits it.
pub fn develop(root: std::path::PathBuf, profile: &str) -> Result<(), SessionError> {
    Session::run(session::Request {
        root,
        profile: profile.to_owned(),
    })
}
