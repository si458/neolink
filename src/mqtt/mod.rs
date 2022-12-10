use log::*;
///
/// # Neolink MQTT
///
/// Handles incoming and outgoing MQTT messages
///
/// # Usage
///
/// ```bash
/// neolink mqtt --config=config.toml
/// ```
///
use std::sync::Arc;

mod app;
mod cmdline;
mod event_cam;
mod mqttc;

use crate::config::{CameraConfig, Config, MqttConfig};
use anyhow::Result;
pub(crate) use app::App;
pub(crate) use cmdline::Opt;
use event_cam::EventCam;
pub(crate) use event_cam::Messages;
use mqttc::{Mqtt, MqttReplyRef};

/// Entry point for the reboot subcommand
///
/// Opt is the command line options
pub(crate) fn main(_: Opt, config: Config) -> Result<()> {
    let app = App::new();
    let arc_app = Arc::new(app);

    let mut mqtt_count: u8 = 0;

    let _ = crossbeam::scope(|s| {
        for camera_config in &config.cameras {
            if let Some(mqtt_config) = camera_config.mqtt.as_ref() {
                let loop_arc_app = arc_app.clone();
                info!("{}: Setting up mqtt", camera_config.name);
                mqtt_count = mqtt_count + 1;
                s.spawn(move |_| {
                    while loop_arc_app.running("app") {
                        let _ = listen_on_camera(camera_config, mqtt_config, loop_arc_app.clone());
                    }
                });
            }
        }
    });

    if mqtt_count == 0 {
        error!("MQTT command run, but no cameras configured with MQTT settings. Exiting.");
    }

    Ok(())
}

fn listen_on_camera(
    cam_config: &CameraConfig,
    mqtt_config: &MqttConfig,
    app: Arc<App>,
) -> Result<()> {
    // Camera thread
    let event_cam = EventCam::new(cam_config, app.clone());
    let mqtt = Mqtt::new(mqtt_config, &cam_config.name, app.clone());

    let _ = crossbeam::scope(|s| {
        // Start listening to camera events
        s.spawn(|_| {
            event_cam.start_listening(); // Loop forever
            event_cam.abort(); // Just to ensure everything aborts
        });

        // Start listening to mqtt events
        s.spawn(|_| {
            let _ = mqtt.start().is_err();
            event_cam.abort();
        });

        // Listen on camera messages and post on mqtt
        s.spawn(|_| {
            while app.running(&format!("app: {}", cam_config.name)) {
                if let Ok(msg) = event_cam.poll() {
                    match msg {
                        Messages::Login => {
                            if mqtt.send_message("status", "connected", true).is_err() {
                                error!("Failed to post connect over MQTT for {}", cam_config.name);
                            }
                        }
                        Messages::MotionStop => {
                            if mqtt.send_message("status/motion", "off", true).is_err() {
                                error!("Failed to publish motion stop for {}", cam_config.name);
                            }
                        }
                        Messages::MotionStart => {
                            if mqtt.send_message("status/motion", "on", true).is_err() {
                                error!("Failed to publish motion start for {}", cam_config.name);
                            }
                        }
                        _ => {}
                    }
                }
            }
        });

        // Listen on mqtt messages and post on camera
        s.spawn(|_| {
            while app.running(&format!("app: {}", cam_config.name)) {
                if let Ok(msg) = mqtt.poll() {
                    match msg.as_ref() {
                        MqttReplyRef {
                            topic: "control/led",
                            message: "on",
                        } => {
                            if event_cam.send_message(Messages::StatusLedOn).is_err() {
                                error!("Failed to set camera status light on");
                            }
                        }
                        MqttReplyRef {
                            topic: "control/led",
                            message: "off",
                        } => {
                            if event_cam.send_message(Messages::StatusLedOff).is_err() {
                                error!("Failed to set camera status light off");
                            }
                        }
                        MqttReplyRef {
                            topic: "control/ir",
                            message: "on",
                        } => {
                            if event_cam.send_message(Messages::IRLedOn).is_err() {
                                error!("Failed to set camera status light off");
                            }
                        }
                        MqttReplyRef {
                            topic: "control/ir",
                            message: "off",
                        } => {
                            if event_cam.send_message(Messages::IRLedOff).is_err() {
                                error!("Failed to set camera status light off");
                            }
                        }
                        MqttReplyRef {
                            topic: "control/ir",
                            message: "auto",
                        } => {
                            if event_cam.send_message(Messages::IRLedAuto).is_err() {
                                error!("Failed to set camera status light off");
                            }
                        }
                        MqttReplyRef {
                            topic: "control/reboot",
                            ..
                        } => {
                            if event_cam.send_message(Messages::Reboot).is_err() {
                                error!("Failed to set camera status light off");
                            }
                        }
                        _ => {}
                    }
                }
            }
        });
    });

    Ok(())
}