//! The host's end of a live preset session.
//!
//! Launching a preset and talking to it is the same work whichever host is doing
//! it, so it is here rather than in either. `zup preset dev` swaps a child process
//! every time a source file changes, and it has to be the *same* create-handshake-
//! publish-read loop a fresh install runs, or a preset author would be developing
//! against a transport that behaves slightly differently from the one their users
//! get.
//!
//! What is deliberately not here is where the preset's bytes came from. An
//! installer reads an embedded package or an installed content store, and a
//! development environment reads whatever its last build produced; each of those
//! resolves the executable and the asset table first, and hands the result to
//! [`launch`].
//!
//! # One lifecycle, both platforms
//!
//! The child is launched through `process-wrap` in a job object on Windows and a
//! process group on Unix, from this one function, so terminating a preset
//! terminates everything that preset started. A preset is a window with a GPU
//! server, asset workers and whatever else its author put behind it, and a host
//! that killed only the process it happened to spawn would leave the rest of the
//! tree running with no window to end it from.
//!
//! `process-wrap` owns the mechanics. What stays here is the policy: a preset the
//! host launched is the host's to finish with, and a launch that does not complete
//! ends the child rather than leaving it unowned.

use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use process_wrap::std::{ChildWrapper, CommandWrap};
use zup_preset_ipc::{Bootstrap, Channel, Endpoint, GREETING_TIMEOUT, Sender};
use zup_preset_protocol::{Capabilities, Envelope, SessionId};

/// Why a session with a preset did not happen.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("the preset process could not be started: {0}")]
    Launch(#[source] std::io::Error),
    #[error("the preset did not complete the handshake: {0}")]
    Handshake(String),
    #[error("the session ended: {0}")]
    Session(String),
}

/// The process, the writing half of the channel, and the protocol `Session` that
/// turns frames into state changes. A preset that exits is not an error here: the
/// installation is exactly where it was, and the host is still the authority on
/// it.
///
/// The child is held for the whole lifetime of the session rather than borrowed,
/// because it is what holds the process tree: a preset that outlived its host's
/// handle would outlive the window that was showing it.
///
/// The reading half is handed to one thread and the writing half stays here,
/// because a session has exactly one reader and any number of writers, and
/// because the operating system's channel is not safe to share across threads
/// even though it is safe to move between them.
pub struct PresetProcess {
    child: Box<dyn ChildWrapper>,
    to_preset: Sender,
    session: Arc<Mutex<zup_preset_protocol::Session>>,
    reading: Option<Channel>,
}

impl PresetProcess {
    /// The reading half, for the thread that reads what the preset asks for.
    ///
    /// Taken once. A second reader would make the session's sequence numbers
    /// describe two interleavings instead of one, so a host has one thread and
    /// one stream of intent.
    pub fn take_reader(&mut self) -> PresetReader {
        PresetReader {
            channel: self
                .reading
                .take()
                .expect("a live session has one reading half"),
            session: Arc::clone(&self.session),
        }
    }

    /// Write one frame to the preset.
    pub fn send(&self, envelope: &Envelope) -> Result<(), SessionError> {
        self.to_preset
            .send(envelope)
            .map_err(|error| SessionError::Session(error.to_string()))
    }

    /// The preset's first frame, which the host answers.
    ///
    /// Read on the calling thread rather than by the reader, because the answer
    /// has to go back before the preset can do anything and the reader is started
    /// afterwards.
    ///
    /// A failure here drops `self`, which ends the child: a preset that would not
    /// open a session is not a preset this host is going to talk to later.
    ///
    /// Bounded, because the preset opens and not the host. A host sitting on this
    /// read is an installer whose window is never going to appear, and the tree it
    /// is holding is a preset nothing will ever replace. The deadline is Zup's:
    /// `process-wrap` says how a tree is ended, not how long a child has to speak.
    pub fn greet(&mut self) -> Result<(), SessionError> {
        let channel = self
            .reading
            .take()
            .expect("a live session has its reading half");
        let (channel, envelope) = channel
            .recv_within(GREETING_TIMEOUT)
            .map_err(|error| SessionError::Handshake(error.to_string()))?;
        self.reading = Some(channel);
        match self.session.lock().expect("the session").receive(envelope) {
            Ok(zup_preset_protocol::SessionProgress::Send(answer)) => {
                self.send(&answer).map_err(|error| {
                    SessionError::Handshake(format!("answering the greeting: {error}"))
                })
            }
            Ok(_) => Err(SessionError::Handshake(
                "the preset did not open with a hello".into(),
            )),
            Err(error) => Err(SessionError::Handshake(error.to_string())),
        }
    }

    /// Tell the preset what the application configured and what the host is
    /// doing, in full.
    ///
    /// Two frames, always, and always in that order: a snapshot is the complete
    /// state, so a preset that starts late or missed a message renders correctly
    /// from what it was handed rather than from a delta it may have missed.
    pub fn publish(
        &self,
        configuration: zup_preset_protocol::Configuration,
        snapshot: Box<zup_preset_protocol::Snapshot>,
    ) -> Result<(), SessionError> {
        let frames = self
            .session
            .lock()
            .expect("the session")
            .publish(configuration, snapshot)
            .map_err(|error| SessionError::Session(error.to_string()))?;
        for frame in frames {
            self.send(&frame)?;
        }
        Ok(())
    }

    /// Whether the preset is still running.
    pub fn is_running(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }
}

impl Drop for PresetProcess {
    fn drop(&mut self) {
        // A preset the host launched is the host's to finish with. Dropping the
        // handle would leave the tree running with nobody waiting for it, and a
        // preset that outlives its session still holds its executable open - which
        // is the file a replacement build would otherwise be unable to write.
        //
        // Ending on drop rather than on an explicit call is the whole design: every
        // path out of a session - a preset that exits, a replacement, a session
        // that goes out of scope, a failure partway through starting one - ends
        // here, so none of them can leave a tree behind. There is deliberately no
        // `shutdown` for a caller to forget.
        end(&mut self.child);
    }
}

/// What a background thread needs in order to read what a preset asks for.
pub struct PresetReader {
    channel: Channel,
    session: Arc<Mutex<zup_preset_protocol::Session>>,
}

impl PresetReader {
    /// The next thing the preset asked for, or the end of the session.
    pub fn next(&self) -> Option<zup_preset_protocol::Action> {
        loop {
            let envelope = self.channel.recv().ok()?;
            match self.session.lock().expect("the session").receive(envelope) {
                Ok(zup_preset_protocol::SessionProgress::Act(action)) => return Some(action),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    }
}

/// Create the endpoint, launch the preset, and wait for it to open the session.
///
/// Synchronous because that is what it is: the host waits on a child starting,
/// and then on the child saying hello.
///
/// The preset opens. That is the SDK's order and the protocol's: a preset says
/// who it is, the host answers with what it provides, and a host that spoke first
/// would have its answer refused as a hello arriving in a session that had
/// already moved on.
///
/// Every failure after the child exists ends it, and that is why the ownership
/// moves across this function rather than at the end of it. The endpoint exists so
/// that the child has something to collect, and a child that starts and never
/// collects - or collects and never says hello - is a process this host started
/// and is not going to be talking to. Handing the child to [`PresetProcess`]
/// before either wait means the remaining failure ends it by drop, so there is no
/// window between the two waits in which a live tree has no owner.
pub fn launch(
    executable: &Path,
    capabilities: Capabilities,
    product: zup_preset_protocol::ProductIdentity,
) -> Result<PresetProcess, SessionError> {
    let endpoint =
        Endpoint::create().map_err(|error| SessionError::Handshake(error.to_string()))?;
    // Before the child exists, so nothing else can take the name.
    let mut command = CommandWrap::with_new(executable, |command| {
        command
            .arg(Bootstrap::to_argument(endpoint.name()))
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
    });
    manage(&mut command);
    let mut child = command.spawn().map_err(SessionError::Launch)?;

    let channel = match endpoint.accept() {
        Ok(channel) => channel,
        Err(error) => {
            end(&mut child);
            return Err(SessionError::Handshake(error.to_string()));
        }
    };

    let mut process = PresetProcess {
        child,
        to_preset: channel.sender().clone(),
        session: Arc::new(Mutex::new(zup_preset_protocol::Session::host(
            SessionId(channel.session()),
            capabilities,
            product,
        ))),
        reading: Some(channel),
    };
    process.greet()?;
    Ok(process)
}

/// Put `command`'s whole process tree under this process's control.
///
/// The two platforms answer the same question differently and this is the only
/// place that knows how: a job object on Windows, a new process group on Unix.
/// Both give one thing to terminate - the tree - so callers above never ask which
/// platform they are on.
fn manage(command: &mut CommandWrap) {
    #[cfg(windows)]
    command.wrap(process_wrap::std::JobObject);
    #[cfg(unix)]
    command.wrap(process_wrap::std::ProcessGroup::leader());
}

/// End a managed child and everything it started, then reap it.
///
/// Both steps are attempted whatever the other reported: terminating a tree that
/// is already gone is an error for a kill nobody needed, and a child whose
/// termination failed still has a handle that has to be released. This is the
/// whole of `PresetProcess`'s lifecycle story, and it is why a failure partway
/// through [`launch`] cannot leave a preset behind.
fn end(child: &mut Box<dyn ChildWrapper>) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Check that this host can present the preset before anything is launched.
///
/// Here rather than only at build time because what a host can present is a fact
/// about the session: an installer that can present a preset on a fresh install
/// can be unable to present the same preset during maintenance, and finding that
/// out before the child exists is the difference between a message and a window
/// that never appears.
pub fn check_presentable(
    preset: &zup_core::PresetRuntime,
    capabilities: &Capabilities,
) -> Result<(), String> {
    let offers = zup_preset_protocol::HostOffers::new(capabilities.clone());
    let required: Capabilities = preset
        .required_capabilities
        .iter()
        .filter_map(|name| zup_preset_protocol::Capability::parse(name))
        .collect();
    offers
        .check(preset.protocol, &required)
        .map_err(|error| error.to_string())
}
