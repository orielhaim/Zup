//! The contract a preset implements, and the one function that starts it.
//!
//! A preset is a normal Rust program that happens to talk to a zup installer.
//! That is the whole idea, and it is why this crate is small: it hands a
//! preset a [`UiSession`], its settings, and its assets, and then gets out of
//! the way. There is no layout abstraction, no screen abstraction, no widget
//! vocabulary, and nothing to subclass - a preset draws with GPUI directly,
//! which means it can draw anything GPUI can draw, including things zup never
//! imagined.
//!
//! ```no_run
//! use zup_ui_sdk::prelude::*;
//!
//! #[derive(Default, serde::Deserialize, schemars::JsonSchema)]
//! struct Settings {
//!     hero: Option<String>,
//!     logo: Option<AssetRef>,
//!     accent: Option<String>,
//! }
//!
//! struct Aurora;
//!
//! impl Preset for Aurora {
//!     const NAME: &'static str = env!("CARGO_PKG_NAME");
//!     const VERSION: &'static str = env!("CARGO_PKG_VERSION");
//!     type Settings = Settings;
//!
//!     fn launch(context: PresetContext<Self::Settings>, cx: &mut App) {
//!         let session = context.session();
//!         // Settings are an entity: they are read through the application, and
//!         // `PresetSettings::observe` is how a preset re-renders when the host
//!         // replaces them.
//!         let accent = context.settings().read(cx).accent.clone().unwrap_or_default();
//!         let _ = (session, accent);
//!     }
//! }
//!
//! fn main() {
//!     zup_ui_sdk::run::<Aurora>();
//! }
//! ```

use futures_channel::mpsc::UnboundedReceiver;
use futures_util::StreamExt;
use gpui_kit::{App, AppContext};
use serde::de::DeserializeOwned;
use zup_ui_protocol::{UiCapabilities, UiMessage};

use crate::asset::{ApplicationAssets, PresetAssets};
use crate::session::UiSession;
use crate::settings::PresetSettings;
use crate::transport::{Bootstrap, Channel, Identity, Opened, TransportError};

/// A preset's typed settings.
///
/// `serde` carries them and `schemars` generates the schema `zup ui pack` puts
/// in the package, so an application's `[ui.settings]` is validated against the
/// preset's own types without the preset being executed and without a second
/// definition language beside Cargo and Rust.
///
/// The bound is `Default` because an application that configures nothing has
/// to work: the host sends an empty document, and a settings type has to be
/// able to be one. That is also why a preset with no settings uses
/// [`NoSettings`] rather than an empty struct with the same effect spelled out
/// again.
pub trait Settings: DeserializeOwned + schemars::JsonSchema + Default + 'static {}

impl<T> Settings for T where T: DeserializeOwned + schemars::JsonSchema + Default + 'static {}

/// Settings for a preset that has none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NoSettings;

impl schemars::JsonSchema for NoSettings {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "NoSettings".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "description": "A preset that takes no configuration.",
            "additionalProperties": false,
        })
    }
}

/// Everything a preset is given, once, at launch.
pub struct PresetContext<TSettings> {
    session: UiSession,
    settings: PresetSettings<TSettings>,
    assets: ApplicationAssets,
    host: zup_ui_protocol::HostHello,
}

impl<TSettings> PresetContext<TSettings> {
    /// The connection to the installer.
    pub fn session(&self) -> &UiSession {
        &self.session
    }

    /// What the application configured, and what it configures next.
    ///
    /// Observable rather than a value, because the host owns it and may replace
    /// it while the session runs. A preset that copied it out at launch would
    /// draw a configuration that no longer exists.
    pub fn settings(&self) -> &PresetSettings<TSettings> {
        &self.settings
    }

    /// The files the application provided.
    pub fn assets(&self) -> &ApplicationAssets {
        &self.assets
    }

    /// What the host says it is.
    pub fn host(&self) -> &zup_ui_protocol::HostHello {
        &self.host
    }
}

/// A native GPUI installer preset.
///
/// One associated type, two identity constants, and one method. Everything else
/// a preset needs is ordinary GPUI, and nothing in this trait is a place a
/// layout decision could hide: `launch` is handed a GPUI `App` and is free to
/// open as many windows and draw as much as it likes.
///
/// ```no_run
/// # use zup_ui_sdk::prelude::*;
/// # struct Aurora;
/// impl Preset for Aurora {
///     const NAME: &'static str = env!("CARGO_PKG_NAME");
///     const VERSION: &'static str = env!("CARGO_PKG_VERSION");
///     type Settings = NoSettings;
///     fn launch(context: PresetContext<Self::Settings>, cx: &mut App) {}
/// }
/// ```
///
/// The two constants are Cargo's own package identity, read with `env!` in the
/// *preset's* crate. `zup ui pack` cross-checks them against the same fields
/// from `cargo metadata`, so a preset whose executable disagrees with its
/// manifest is refused rather than packaged under a name nobody can trace.
pub trait Preset: 'static {
    /// This preset's Cargo package name.
    const NAME: &'static str;
    /// This preset's Cargo package version.
    const VERSION: &'static str;

    /// This preset's typed settings.
    type Settings: Settings;

    /// What this preset cannot present without.
    ///
    /// A host refuses a preset that requires something it does not provide,
    /// before the preset is launched, so a missing capability is a clear
    /// message rather than a dead control. The default is nothing: a preset
    /// that can present whatever a host has should say so.
    fn required_capabilities() -> UiCapabilities {
        UiCapabilities::default()
    }

    /// Draw the installer.
    ///
    /// Called once the handshake is done and the first snapshot has arrived,
    /// so `context.session().state()` already has the state to draw.
    fn launch(context: PresetContext<Self::Settings>, cx: &mut App);
}

/// Why a preset stopped before it drew anything.
#[derive(Debug, thiserror::Error)]
pub enum PresetError {
    #[error(
        "this program is a zup installer preset; it is launched by an installer, not run directly"
    )]
    NotLaunchedByAHost,
    #[error(transparent)]
    Describe(#[from] Describe),
    #[error("the installer's settings do not fit this preset: {0}")]
    Settings(String),
    #[error(transparent)]
    Transport(#[from] TransportError),
}

/// Why a preset could not say what it is.
///
/// A publisher runs the preset's describe mode and gets a refusal rather than a
/// document that would fail later, where the author is not watching.
#[derive(Debug, thiserror::Error)]
pub enum Describe {
    #[error("this preset's settings are not a JSON Schema: {0}")]
    Settings(#[from] serde_json::Error),
    #[error(transparent)]
    Unusable(#[from] zup_ui_protocol::DescribeError),
}

/// Run a preset.
///
/// The whole of a preset's `main`. It describes the preset when a build asks it
/// to, and otherwise connects to the host it was launched for, completes the
/// handshake, waits for the first state, and hands the session to
/// [`Preset::launch`].
///
/// `run` returns rather than exiting, so a test can call it and see what
/// happened; turning a failure into an exit code is a preset's `main`'s job.
pub fn run<P: Preset>() -> Result<(), PresetError> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments
        .iter()
        .any(|argument| argument == zup_ui_protocol::DESCRIBE_FLAG)
    {
        return match describe::<P>() {
            Ok(document) => {
                println!("{document}");
                Ok(())
            }
            Err(error) => Err(PresetError::Describe(error)),
        };
    }
    let bootstrap = Bootstrap::from_arguments(arguments).map_err(|error| match error {
        TransportError::NotLaunchedByAHost => PresetError::NotLaunchedByAHost,
        other => PresetError::Transport(other),
    })?;
    serve::<P>(bootstrap)
}

/// Print what this preset is, for `zup ui pack`.
///
/// A build and publishing concern, and the only time a preset executable runs.
/// The schema is generated from the preset's own `Settings` type, so the
/// document cannot drift from the types the preset will deserialize at runtime
/// and there is no second definition language to keep in step.
///
/// The document is checked here rather than left to the publisher, so a preset
/// that could not be packaged says so from the one command its author ran.
pub fn describe<P: Preset>() -> Result<String, Describe> {
    let schema = schemars::schema_for!(P::Settings);
    let description =
        zup_ui_protocol::PresetDescription::new(P::NAME, P::VERSION, serde_json::to_value(schema)?)
            .with_capabilities(P::required_capabilities());
    description.validate()?;
    Ok(serde_json::to_string_pretty(&description)?)
}

/// Connect to the host, wait for the first state, and draw.
pub fn serve<P: Preset>(bootstrap: Bootstrap) -> Result<(), PresetError> {
    let identity = Identity {
        name: P::NAME.to_owned(),
        version: P::VERSION.to_owned(),
        required_capabilities: P::required_capabilities(),
    };
    let Opened {
        channel,
        host,
        configuration,
        snapshot: first,
    } = Channel::open(&bootstrap, identity)?;
    let settings: P::Settings = decode(&configuration.settings, "the host's first settings")?;

    let source_assets = ApplicationAssets::from_configuration(&configuration);
    let assets = source_assets.clone();
    let (incoming, requester) = channel.into_parts();
    gpui_kit::application()
        .with_assets(PresetAssets::new(source_assets))
        .run(move |cx| {
            gpui_kit::init(cx);
            let session = UiSession::open(cx, requester);
            let settings = PresetSettings::new(cx.new(|_| settings));
            // Seeded through the application this closure already holds, before the
            // preset is handed its context: a window's first draw should be the
            // state the host published rather than an empty one.
            session.publish(cx, first);
            let pumped = session.clone();
            let pumped_assets = assets.clone();
            let pumped_settings = settings.clone();
            cx.spawn(async move |cx: &mut gpui_kit::AsyncApp| {
                pump::<P::Settings>(incoming, &pumped, &pumped_assets, &pumped_settings, cx).await
            })
            .detach();
            P::launch(
                PresetContext {
                    session,
                    settings,
                    assets,
                    host,
                },
                cx,
            );
        });
    Ok(())
}

/// Move what the host says into the session, for as long as it says anything.
///
/// A GPUI task rather than a plain thread: applying a snapshot means writing to
/// an entity, and an entity belongs to the application.
async fn pump<TSettings: Settings>(
    mut incoming: UnboundedReceiver<Result<UiMessage, TransportError>>,
    session: &UiSession,
    assets: &ApplicationAssets,
    settings: &PresetSettings<TSettings>,
    cx: &mut gpui_kit::AsyncApp,
) {
    while let Some(message) = incoming.next().await {
        match message {
            Ok(UiMessage::Snapshot(snapshot)) => session.publish_from_task(cx, *snapshot),
            Ok(UiMessage::Configuration(configuration)) => {
                let zup_ui_protocol::UiConfiguration {
                    settings: document,
                    assets: files,
                } = *configuration;
                assets.replace(files);
                if let Ok(decoded) = decode(&document, "the host's settings") {
                    settings.replace(cx, decoded);
                }
                session.refresh(cx);
            }
            Ok(UiMessage::Closed) | Err(_) => {
                session.disconnect(cx, None);
                return;
            }
            Ok(_) => continue,
        }
    }
    session.disconnect(cx, None);
}

/// The host's settings, as this preset's own type.
///
/// A configuration whose settings do not fit is not an error a preset can
/// recover from by stopping: the host is still there, the window is still open,
/// and the last settings that did fit are the only ones a person can be shown.
/// So this reports a refusal to the caller's caller rather than raising, and
/// `pump` keeps what it has.
fn decode<S: Settings>(document: &serde_json::Value, context: &str) -> Result<S, PresetError> {
    serde_json::from_value(document.clone()).map_err(|error| {
        PresetError::Settings(format!("{} ({context}): {error}", S::schema_name()))
    })
}
