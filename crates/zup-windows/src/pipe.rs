//! Real Tokio named-pipe transport with explicit ACL.
//!
//! Parent: `ServerOptions` with `first_pipe_instance`, `reject_remote_clients`,
//! `max_instances(1)`, and an explicit `SECURITY_ATTRIBUTES` DACL allowing
//! only: initiating user SID, BUILTIN\Administrators, SYSTEM.
//!
//! Worker may be a *different* administrator account (over-the-shoulder UAC).
//! Authentication is exact process binding, not SID equality.

use std::os::windows::io::{AsRawHandle, RawHandle};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

use crate::transport::{PipeSecurityDescriptor, TransportError, UserSid, verify_client_pid};
use zup_protocol::{
    MAX_FRAME_BYTES, PROTOCOL_VERSION, WireEnvelope, decode_payload, encode_payload,
};

/// Transport / connection errors.
#[derive(Debug, Error)]
pub enum PipeError {
    #[error("pipe creation failed: {0}")]
    PipeCreationFailed(String),

    #[error("security descriptor failed: {0}")]
    SecurityDescriptorFailed(String),

    #[error("worker connect timeout")]
    WorkerConnectTimeout,

    #[error("pipe I/O failed: {0}")]
    Io(String),

    #[error("malformed protocol: {0}")]
    MalformedProtocol(String),

    #[error("frame too large")]
    FrameTooLarge,

    #[error("protocol version mismatch")]
    ProtocolVersionMismatch,
}

/// Timeouts (no timeout on installation execution itself).
pub const WORKER_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(15);

/// Build the DACL backing allocation. Must outlive `SECURITY_ATTRIBUTES`.
pub struct PipeSecurity {
    descriptor: PipeSecurityDescriptor,
}

impl PipeSecurity {
    /// Create a descriptor allowing: initiating user SID, Administrators, SYSTEM.
    pub fn for_initiating_user() -> Result<Self, PipeError> {
        let user =
            UserSid::current().map_err(|e| PipeError::SecurityDescriptorFailed(e.to_string()))?;
        let descriptor = PipeSecurityDescriptor::new(&user)
            .map_err(|e| PipeError::SecurityDescriptorFailed(e.to_string()))?;
        Ok(Self { descriptor })
    }

    pub fn raw(&self) -> RawHandle {
        self.descriptor.as_raw()
    }
}

/// Parent-side secured named-pipe server.
pub struct PipeServer {
    inner: NamedPipeServer,
    name: String,
}

impl PipeServer {
    /// Create the pipe **before** launching the worker.
    pub fn create(name: &str) -> Result<Self, PipeError> {
        let security = PipeSecurity::for_initiating_user()?;
        let path = format!(r"\\.\pipe\{name}");
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .max_instances(1);

        // SAFETY: `security` outlives this call; the descriptor is not used
        // after `create_with_security_attributes_raw` returns because Tokio
        // copies what it needs into the pipe handle.
        let inner = unsafe {
            options
                .create_with_security_attributes_raw(&path, security.raw())
                .map_err(|e| PipeError::PipeCreationFailed(e.to_string()))?
        };
        // Keep security alive until the pipe exists.
        drop(security);
        Ok(Self {
            inner,
            name: name.to_owned(),
        })
    }

    /// Wait for the worker client to connect.
    pub async fn connect(&mut self) -> Result<(), PipeError> {
        self.inner
            .connect()
            .await
            .map_err(|e| PipeError::Io(e.to_string()))
    }

    pub async fn connect_worker(&mut self, expected_pid: u32) -> Result<(), PipeError> {
        loop {
            tokio::time::timeout(WORKER_CONNECT_TIMEOUT, self.inner.connect())
                .await
                .map_err(|_| PipeError::WorkerConnectTimeout)?
                .map_err(|error| PipeError::Io(error.to_string()))?;
            match verify_client_pid(self.as_raw() as isize, expected_pid) {
                Ok(()) => return Ok(()),
                Err(TransportError::WorkerPidMismatch { .. }) => {
                    self.inner
                        .disconnect()
                        .map_err(|error| PipeError::Io(error.to_string()))?;
                }
                Err(error) => return Err(PipeError::Io(error.to_string())),
            }
        }
    }

    /// Raw handle for PID/session queries.
    pub fn as_raw(&self) -> RawHandle {
        self.inner.as_raw_handle()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn into_inner(self) -> Option<NamedPipeServer> {
        Some(self.inner)
    }
}

/// Worker-side client with bounded connect retries.
pub struct PipeClient {
    inner: NamedPipeClient,
}

impl PipeClient {
    /// Connect, retrying only transient pipe-not-ready errors.
    pub async fn connect(name: &str) -> Result<Self, PipeError> {
        let path = format!(r"\\.\pipe\{name}");
        let deadline = tokio::time::Instant::now() + WORKER_CONNECT_TIMEOUT;
        loop {
            match ClientOptions::new().open(&path) {
                Ok(inner) => return Ok(Self { inner }),
                Err(e) => {
                    let transient = matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound
                            | std::io::ErrorKind::ConnectionRefused
                            | std::io::ErrorKind::WouldBlock
                    );
                    if !transient || tokio::time::Instant::now() >= deadline {
                        return Err(PipeError::WorkerConnectTimeout);
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }

    pub fn as_raw(&self) -> RawHandle {
        self.inner.as_raw_handle()
    }

    pub fn into_inner(self) -> NamedPipeClient {
        self.inner
    }
}

/// Build framed halves from a connected server.
pub fn frame_server(server: NamedPipeServer) -> (ServerReader, ServerWriter) {
    let (read, write) = tokio::io::split(server);
    (
        ServerReader {
            inner: FramedRead::new(
                read,
                LengthDelimitedCodec::builder()
                    .max_frame_length(MAX_FRAME_BYTES)
                    .new_codec(),
            ),
        },
        ServerWriter {
            inner: FramedWrite::new(
                write,
                LengthDelimitedCodec::builder()
                    .max_frame_length(MAX_FRAME_BYTES)
                    .new_codec(),
            ),
        },
    )
}

/// Build framed halves from a connected client.
pub fn frame_client(client: NamedPipeClient) -> (ClientReader, ClientWriter) {
    let (read, write) = tokio::io::split(client);
    (
        ClientReader {
            inner: FramedRead::new(
                read,
                LengthDelimitedCodec::builder()
                    .max_frame_length(MAX_FRAME_BYTES)
                    .new_codec(),
            ),
        },
        ClientWriter {
            inner: FramedWrite::new(
                write,
                LengthDelimitedCodec::builder()
                    .max_frame_length(MAX_FRAME_BYTES)
                    .new_codec(),
            ),
        },
    )
}

/// Server-side framed reader.
pub struct ServerReader {
    inner: FramedRead<tokio::io::ReadHalf<NamedPipeServer>, LengthDelimitedCodec>,
}

/// Server-side framed writer.
pub struct ServerWriter {
    inner: FramedWrite<tokio::io::WriteHalf<NamedPipeServer>, LengthDelimitedCodec>,
}

/// Client-side framed reader.
pub struct ClientReader {
    inner: FramedRead<tokio::io::ReadHalf<NamedPipeClient>, LengthDelimitedCodec>,
}

/// Client-side framed writer.
pub struct ClientWriter {
    inner: FramedWrite<tokio::io::WriteHalf<NamedPipeClient>, LengthDelimitedCodec>,
}

macro_rules! impl_recv_send {
    ($reader:ident, $writer:ident) => {
        impl $reader {
            pub async fn recv(&mut self) -> Result<WireEnvelope, PipeError> {
                let frame = self
                    .inner
                    .next()
                    .await
                    .ok_or_else(|| PipeError::Io("pipe closed".into()))?
                    .map_err(|e| PipeError::MalformedProtocol(e.to_string()))?;
                decode_payload(&frame).map_err(|e| match e {
                    zup_protocol::WireError::FrameTooLarge { .. } => PipeError::FrameTooLarge,
                    zup_protocol::WireError::VersionMismatch { .. } => {
                        PipeError::ProtocolVersionMismatch
                    }
                    other => PipeError::MalformedProtocol(other.to_string()),
                })
            }
        }

        impl $writer {
            pub async fn send(&mut self, envelope: &WireEnvelope) -> Result<(), PipeError> {
                let bytes = encode_payload(envelope)
                    .map_err(|e| PipeError::MalformedProtocol(e.to_string()))?;
                self.inner
                    .send(bytes.into())
                    .await
                    .map_err(|e| PipeError::Io(e.to_string()))
            }
        }
    };
}

impl_recv_send!(ServerReader, ServerWriter);
impl_recv_send!(ClientReader, ClientWriter);

/// Validate the protocol version of an envelope.
pub fn check_version(envelope: &WireEnvelope) -> Result<(), PipeError> {
    if envelope.version != PROTOCOL_VERSION {
        return Err(PipeError::ProtocolVersionMismatch);
    }
    Ok(())
}
