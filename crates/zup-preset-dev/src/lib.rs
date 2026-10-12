#![deny(unsafe_code)]

mod build;
mod development;
mod project;
mod session;
mod watch;

pub use build::{Build, Building, Diagnostic, Supervisor};
pub use development::{Development, DevelopmentError, FILE_NAME};
pub use project::{Project, ProjectError};
pub use session::{Request, Session, SessionError};
pub use watch::{Change, Watched, Watcher};

pub fn develop(root: std::path::PathBuf, profile: &str) -> Result<(), SessionError> {
    Session::run(session::Request {
        root,
        profile: profile.to_owned(),
    })
}
