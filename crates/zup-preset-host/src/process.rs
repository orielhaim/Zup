use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use process_wrap::std::{ChildWrapper, CommandWrap};
use zup_preset_ipc::{Bootstrap, Channel, Endpoint, GREETING_TIMEOUT, Sender};
use zup_preset_protocol::{Capabilities, Envelope, SessionId};

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("the preset process could not be started: {0}")]
    Launch(#[source] std::io::Error),
    #[error("the preset did not complete the handshake: {0}")]
    Handshake(String),
    #[error("the session ended: {0}")]
    Session(String),

    #[error("the preset closed its session: {0}")]
    Disconnected(String),
}

pub struct PresetProcess {
    child: Box<dyn ChildWrapper>,
    to_preset: Sender,
    session: Arc<Mutex<zup_preset_protocol::Session>>,
    reading: Option<Channel>,
}

impl PresetProcess {
    pub fn take_reader(&mut self) -> PresetReader {
        PresetReader {
            channel: self
                .reading
                .take()
                .expect("a live session has one reading half"),
            session: Arc::clone(&self.session),
        }
    }

    pub fn send(&self, envelope: &Envelope) -> Result<(), SessionError> {
        self.to_preset.send(envelope).map_err(|error| {
            if error.peer_gone() {
                SessionError::Disconnected(error.to_string())
            } else {
                SessionError::Session(error.to_string())
            }
        })
    }

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

    pub fn is_running(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }
}

impl Drop for PresetProcess {
    fn drop(&mut self) {
        end(&mut self.child);
    }
}

pub struct PresetReader {
    channel: Channel,
    session: Arc<Mutex<zup_preset_protocol::Session>>,
}

impl PresetReader {
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

pub fn launch(
    executable: &Path,
    capabilities: Capabilities,
    product: zup_preset_protocol::ProductIdentity,
) -> Result<PresetProcess, SessionError> {
    let endpoint =
        Endpoint::create().map_err(|error| SessionError::Handshake(error.to_string()))?;
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

fn manage(command: &mut CommandWrap) {
    #[cfg(windows)]
    command.wrap(process_wrap::std::JobObject);
    #[cfg(unix)]
    command.wrap(process_wrap::std::ProcessGroup::leader());
}

fn end(child: &mut Box<dyn ChildWrapper>) {
    let _ = child.kill();
    let _ = child.wait();
}

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
