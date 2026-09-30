//! The host's end of a live preset session.
//!
//! Launching a preset and talking to it is the same work whichever host is doing
//! it, so it is here rather than in either. `zup ui dev` swaps a child process
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

use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use zup_ui_ipc::{Bootstrap, Channel, Endpoint, Sender};
use zup_ui_protocol::{UiCapabilities, UiEnvelope, UiSessionId};

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
/// The reading half is handed to one thread and the writing half stays here,
/// because a session has exactly one reader and any number of writers, and
/// because the operating system's channel is not safe to share across threads
/// even though it is safe to move between them.
pub struct PresetProcess {
    child: std::process::Child,
    to_preset: Sender,
    session: Arc<Mutex<zup_ui_protocol::Session>>,
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
    pub fn send(&self, envelope: &UiEnvelope) -> Result<(), SessionError> {
        self.to_preset
            .send(envelope)
            .map_err(|error| SessionError::Session(error.to_string()))
    }

    /// The preset's first frame, which the host answers.
    ///
    /// Read on the calling thread rather than by the reader, because the answer
    /// has to go back before the preset can do anything and the reader is started
    /// afterwards.
    pub fn greet(&mut self) -> Result<(), SessionError> {
        let envelope = self
            .reading
            .as_ref()
            .expect("a live session has its reading half")
            .recv()
            .map_err(|error| SessionError::Handshake(error.to_string()))?;
        match self.session.lock().expect("the session").receive(envelope) {
            Ok(zup_ui_protocol::SessionProgress::Send(answer)) => self.send(&answer),
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
        configuration: zup_ui_protocol::UiConfiguration,
        snapshot: Box<zup_ui_protocol::UiSnapshot>,
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

    /// End the session: the child stops, and its handle is reaped.
    ///
    /// A preset the host launched is the host's to finish with. Dropping the
    /// handle would leave the process running with nobody waiting for it, and a
    /// preset that outlives its session still holds its executable open - which
    /// is the file a replacement build would otherwise be unable to write.
    pub fn shutdown(&mut self) {
        if self.is_running() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

impl Drop for PresetProcess {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// What a background thread needs in order to read what a preset asks for.
pub struct PresetReader {
    channel: Channel,
    session: Arc<Mutex<zup_ui_protocol::Session>>,
}

impl PresetReader {
    /// The next thing the preset asked for, or the end of the session.
    pub fn next(&self) -> Option<zup_ui_protocol::UiAction> {
        loop {
            let envelope = self.channel.recv().ok()?;
            match self.session.lock().expect("the session").receive(envelope) {
                Ok(zup_ui_protocol::SessionProgress::Act(action)) => return Some(action),
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
pub fn launch(
    executable: &Path,
    capabilities: UiCapabilities,
    product: zup_ui_protocol::ProductIdentity,
) -> Result<PresetProcess, SessionError> {
    let endpoint =
        Endpoint::create().map_err(|error| SessionError::Handshake(error.to_string()))?;
    // Before the child exists, so nothing else can take the name.
    let child = std::process::Command::new(executable)
        .arg(Bootstrap::to_argument(endpoint.name()))
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(SessionError::Launch)?;

    let channel = endpoint
        .accept()
        .map_err(|error| SessionError::Handshake(error.to_string()))?;

    let mut process = PresetProcess {
        child,
        to_preset: channel.sender().clone(),
        session: Arc::new(Mutex::new(zup_ui_protocol::Session::host(
            UiSessionId(channel.session()),
            capabilities,
            product,
        ))),
        reading: Some(channel),
    };
    process.greet()?;
    Ok(process)
}

/// Check that this host can present the preset before anything is launched.
///
/// Here rather than only at build time because what a host can present is a fact
/// about the session: an installer that can present a preset on a fresh install
/// can be unable to present the same preset during maintenance, and finding that
/// out before the child exists is the difference between a message and a window
/// that never appears.
pub fn check_presentable(
    preset: &zup_core::UiPreset,
    capabilities: &UiCapabilities,
) -> Result<(), String> {
    let offers = zup_ui_protocol::HostOffers::new(capabilities.clone());
    let required: UiCapabilities = preset
        .required_capabilities
        .iter()
        .filter_map(|name| zup_ui_protocol::UiCapability::parse(name))
        .collect();
    offers
        .check(preset.protocol, &required)
        .map_err(|error| error.to_string())
}
