#[cfg(feature = "alsa_backend")]
use crate::alsa_mixer;
use crate::{config, main_loop, no_mixer};
use futures::{self, Future};
#[cfg(feature = "dbus_keyring")]
use keyring::Keyring;
use librespot::{
    connect::discovery::discovery,
    core::{
        authentication::get_credentials,
        cache::Cache,
        config::{ConnectConfig, DeviceType, VolumeCtrl},
        session::Session,
    },
    playback::{
        audio_backend::{Sink, BACKENDS},
        mixer::{self, Mixer},
    },
};
use log::{error, info};
use std::str::FromStr;
use std::{io, process::exit};
use tokio_core::reactor::Handle;
use tokio_signal::ctrl_c;

pub(crate) fn initial_state(
    handle: Handle,
    config: config::SpotifydConfig,
) -> main_loop::MainLoopState {
    #[cfg(feature = "alsa_backend")]
    let mut mixer = {
        let local_audio_device = config.audio_device.clone();
        let local_control_device = config.control_device.clone();
        let local_mixer = config.mixer.clone();
        match config.volume_controller {
            config::VolumeController::SoftVolume => {
                info!("Using software volume controller.");
                Box::new(|| Box::new(mixer::softmixer::SoftMixer::open(None)) as Box<dyn Mixer>)
                    as Box<dyn FnMut() -> Box<dyn Mixer>>
            }
            config::VolumeController::None => {
                info!("Using no volume controller.");
                Box::new(|| Box::new(no_mixer::NoMixer::open(None)) as Box<dyn Mixer>)
                    as Box<dyn FnMut() -> Box<dyn Mixer>>
            }
            _ => {
                info!("Using alsa volume controller.");
                let linear = matches!(
                    config.volume_controller,
                    config::VolumeController::AlsaLinear
                );
                Box::new(move || {
                    Box::new(alsa_mixer::AlsaMixer {
                        device: local_control_device
                            .clone()
                            .or_else(|| local_audio_device.clone())
                            .unwrap_or_else(|| "default".to_string()),
                        mixer: local_mixer.clone().unwrap_or_else(|| "Master".to_string()),
                        linear_scaling: linear,
                    }) as Box<dyn mixer::Mixer>
                }) as Box<dyn FnMut() -> Box<dyn Mixer>>
            }
        }
    };

    #[cfg(not(feature = "alsa_backend"))]
    let mut mixer = match config.volume_controller {
        config::VolumeController::None => {
            info!("Using no volume controller.");
            Box::new(|| Box::new(no_mixer::NoMixer::open(None)) as Box<dyn Mixer>)
                as Box<dyn FnMut() -> Box<dyn Mixer>>
        }
        _ => {
            info!("Using software volume controller.");
            Box::new(|| Box::new(mixer::softmixer::SoftMixer::open(None)) as Box<dyn Mixer>)
                as Box<dyn FnMut() -> Box<dyn Mixer>>
        }
    };

    let cache = config.cache;
    let player_config = config.player_config;
    let session_config = config.session_config;
    let backend = config.backend.clone();
    let autoplay = config.autoplay;
    let device_id = session_config.device_id.clone();

    #[cfg(feature = "alsa_backend")]
    let volume_ctrl = if matches!(
        config.volume_controller,
        config::VolumeController::AlsaLinear
    ) {
        VolumeCtrl::Linear
    } else {
        VolumeCtrl::default()
    };

    #[cfg(not(feature = "alsa_backend"))]
    let volume_ctrl = VolumeCtrl::default();

    let zeroconf_port = config.zeroconf_port.unwrap_or(0);

    let device_type: DeviceType = DeviceType::from_str(&config.device_type).unwrap_or_default();

    #[allow(clippy::or_fun_call)]
    let discovery_stream = discovery(
        &handle,
        ConnectConfig {
            autoplay,
            name: config.device_name.clone(),
            device_type,
            volume: mixer().volume(),
            volume_ctrl: volume_ctrl.clone(),
        },
        device_id,
        zeroconf_port,
    )
    .unwrap();

    let username = config.username;
    #[allow(unused_mut)] // mut is needed behind the dbus_keyring flag.
    let mut password = config.password;
    #[cfg(feature = "dbus_keyring")]
    {
        // We only need to check if an actual user has been specified as
        // spotifyd can run without being signed in too.
        if username.is_some() && config.use_keyring {
            info!("Checking keyring for password");
            let keyring = Keyring::new("spotifyd", username.as_ref().unwrap());
            let retrieved_password = keyring.get_password();
            password = password.or_else(|| retrieved_password.ok());
        }
    }

    let connection = if let Some(credentials) = get_credentials(
        username,
        password,
        cache.as_ref().and_then(Cache::credentials),
        |_| {
            error!("No password found.");
            exit(1);
        },
    ) {
        Session::connect(
            session_config.clone(),
            credentials,
            cache.clone(),
            handle.clone(),
        )
    } else {
        Box::new(futures::future::empty())
            as Box<dyn futures::Future<Item = Session, Error = io::Error>>
    };

    let backend = find_backend(backend.as_ref().map(String::as_ref));
    main_loop::MainLoopState {
        librespot_connection: main_loop::LibreSpotConnection::new(connection, discovery_stream),
        audio_setup: main_loop::AudioSetup {
            mixer,
            backend,
            audio_device: config.audio_device.clone(),
        },
        spotifyd_state: main_loop::SpotifydState {
            ctrl_c_stream: Box::new(ctrl_c(&handle).flatten_stream()),
            shutting_down: false,
            cache,
            device_name: config.device_name,
            player_event_channel: None,
            player_event_program: config.onevent,
            dbus_mpris_server: None,
        },
        player_config,
        session_config,
        handle,
        initial_volume: config.initial_volume,
        volume_ctrl,
        running_event_program: None,
        shell: config.shell,
        device_type,
        autoplay,
        use_mpris: config.use_mpris,
    }
}

fn find_backend(name: Option<&str>) -> fn(Option<String>) -> Box<dyn Sink> {
    match name {
        Some(name) => {
            BACKENDS
                .iter()
                .find(|backend| name == backend.0)
                .unwrap_or_else(|| panic!("Unknown backend: {}.", name))
                .1
        }
        None => {
            let &(name, back) = BACKENDS
                .first()
                .expect("No backends were enabled at build time");
            info!("No backend specified, defaulting to: {}.", name);
            back
        }
    }
}
