use futures_channel::mpsc::UnboundedReceiver;
use futures_util::StreamExt;
use gpui_kit::{App, AppContext, AssetSource};
use serde::de::DeserializeOwned;
use zup_preset_protocol::{Capabilities, Message};

use super::asset::{ApplicationAssets, PresetAssets};
use super::session::Session;
use super::settings::PresetSettings;
use super::transport::{Bootstrap, Channel, Identity, Opened, TransportError};

pub trait Settings: DeserializeOwned + schemars::JsonSchema + Default + 'static {}

impl<T> Settings for T where T: DeserializeOwned + schemars::JsonSchema + Default + 'static {}

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

pub struct PresetContext<TSettings> {
    session: Session,
    settings: PresetSettings<TSettings>,
    assets: ApplicationAssets,
    capabilities: Capabilities,
}

impl<TSettings> PresetContext<TSettings> {
    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn settings(&self) -> &PresetSettings<TSettings> {
        &self.settings
    }

    pub fn assets(&self) -> &ApplicationAssets {
        &self.assets
    }

    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }
}

pub trait Preset: 'static {
    const NAME: &'static str;
    const VERSION: &'static str;

    type Settings: Settings;

    fn required_capabilities() -> Capabilities {
        Capabilities::default()
    }

    fn assets() -> impl AssetSource {}

    fn launch(context: PresetContext<Self::Settings>, cx: &mut App);
}

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

#[derive(Debug, thiserror::Error)]
pub enum Describe {
    #[error("this preset's settings are not a JSON Schema: {0}")]
    Settings(#[from] serde_json::Error),
    #[error(transparent)]
    Unusable(#[from] zup_preset_protocol::DescribeError),
}

pub fn run<P: Preset>() -> Result<(), PresetError> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments
        .iter()
        .any(|argument| argument == zup_preset_protocol::DESCRIBE_FLAG)
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

pub fn describe<P: Preset>() -> Result<String, Describe> {
    let schema = schemars::schema_for!(P::Settings);
    let description = zup_preset_protocol::PresetDescription::new(
        P::NAME,
        P::VERSION,
        serde_json::to_value(schema)?,
    )
    .with_capabilities(P::required_capabilities());
    description.validate()?;
    Ok(serde_json::to_string_pretty(&description)?)
}

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
        .with_assets(PresetAssets::new(source_assets, P::assets()))
        .run(move |cx| {
            gpui_kit::init(cx);
            let session = Session::open(cx, requester);
            let settings = PresetSettings::new(cx.new(|_| settings));
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
                    capabilities: host.capabilities,
                },
                cx,
            );
        });
    Ok(())
}

async fn pump<TSettings: Settings>(
    mut incoming: UnboundedReceiver<Result<Message, TransportError>>,
    session: &Session,
    assets: &ApplicationAssets,
    settings: &PresetSettings<TSettings>,
    cx: &mut gpui_kit::AsyncApp,
) {
    while let Some(message) = incoming.next().await {
        match message {
            Ok(Message::Snapshot(snapshot)) => session.publish_from_task(cx, *snapshot),
            Ok(Message::Configuration(configuration)) => {
                let zup_preset_protocol::Configuration {
                    settings: document,
                    assets: files,
                } = *configuration;
                assets.replace(files);
                if let Ok(decoded) = decode(&document, "the host's settings") {
                    settings.replace(cx, decoded);
                }
                session.refresh(cx);
            }
            Ok(Message::Closed) | Err(_) => {
                session.disconnect(cx, None);
                return;
            }
            Ok(_) => continue,
        }
    }
    session.disconnect(cx, None);
}

fn decode<S: Settings>(document: &serde_json::Value, context: &str) -> Result<S, PresetError> {
    serde_json::from_value(document.clone()).map_err(|error| {
        PresetError::Settings(format!("{} ({context}): {error}", S::schema_name()))
    })
}
