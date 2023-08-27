//! This is the common code for creating a camera instance
//!
//! Features:
//!    Shared stream BC delivery
//!    Common restart code
//!    Clonable interface to share amongst threadsanyhow::anyhow;
use futures::stream::StreamExt;
use std::sync::Weak;
use tokio::sync::{
    mpsc::{channel as mpsc, Sender as MpscSender},
    oneshot::{channel as oneshot, Sender as OneshotSender},
    watch::{channel as watch, Receiver as WatchReceiver, Sender as WatchSender},
};
use tokio_stream::wrappers::{BroadcastStream, ReceiverStream};
use tokio_util::sync::CancellationToken;

use super::{NeoCamStreamThread, NeoCamThread, NeoInstance, StreamRequest};
use crate::{config::CameraConfig, Result};
use neolink_core::{
    bc_protocol::{BcCamera, StreamKind},
    bcmedia::model::BcMedia,
};

pub(crate) enum NeoCamCommand {
    HangUp,
    Instance(OneshotSender<Result<NeoInstance>>),
    Stream(StreamKind, OneshotSender<BroadcastStream<BcMedia>>),
}
/// The underlying camera binding
pub(crate) struct NeoCam {
    cancel: CancellationToken,
    config_watch: WatchSender<CameraConfig>,
    commander: MpscSender<NeoCamCommand>,
    camera_watch: WatchReceiver<Weak<BcCamera>>,
}

impl NeoCam {
    pub(crate) async fn new(config: CameraConfig) -> Result<NeoCam> {
        let (commander_tx, commander_rx) = mpsc(100);
        let (watch_config_tx, watch_config_rx) = watch(config.clone());
        let (camera_watch_tx, camera_watch_rx) = watch(Weak::new());
        let (stream_request_tx, stream_request_rx) = mpsc(100);

        let me = Self {
            cancel: CancellationToken::new(),
            config_watch: watch_config_tx,
            commander: commander_tx.clone(),
            camera_watch: camera_watch_rx.clone(),
        };

        // This thread recieves messages from the instances
        // and acts on it.
        //
        // This thread must be started first so that we can begin creating instances for the
        // other threads
        let sender_cancel = me.cancel.clone();
        let mut commander_rx = ReceiverStream::new(commander_rx);
        let strict = config.strict;
        let thread_commander_tx = commander_tx.clone();
        tokio::task::spawn(async move {
            let thread_cancel = sender_cancel.clone();
            let res = tokio::select! {
                _ = sender_cancel.cancelled() => Result::Ok(()),
                v = async {
                    while let Some(command) = commander_rx.next().await {
                        match command {
                            NeoCamCommand::HangUp => {
                                sender_cancel.cancel();
                                log::debug!("Cancel:: NeoCamCommand::HangUp");
                                return Result::<(), anyhow::Error>::Ok(());
                            }
                            NeoCamCommand::Instance(result) => {
                                let instance = NeoInstance::new(
                                    camera_watch_rx.clone(),
                                    thread_commander_tx.clone(),
                                    thread_cancel.clone(),
                                );
                                let _ = result.send(instance);
                            }
                            NeoCamCommand::Stream(name, sender) => {
                                stream_request_tx.send(
                                    StreamRequest {
                                        name,
                                        sender,
                                        strict,
                                    }
                                ).await?;
                            },
                        }
                    }
                    Ok(())
                } => v
            };
            log::debug!("Control thread terminated");
            res
        });

        let mut cam_thread =
            NeoCamThread::new(watch_config_rx, camera_watch_tx, me.cancel.clone()).await;

        // This thread maintains the camera loop
        //
        // It will keep it logged and reconnect
        tokio::task::spawn(async move { cam_thread.run().await });

        let (instance_tx, instance_rx) = oneshot();
        commander_tx
            .send(NeoCamCommand::Instance(instance_tx))
            .await?;

        let instance = instance_rx.await??;

        // This thread maintains the streams
        let stream_instance = instance.subscribe().await?;
        let stream_cancel = me.cancel.clone();
        let mut stream_thread =
            NeoCamStreamThread::new(stream_request_rx, stream_instance, stream_cancel).await;

        tokio::task::spawn(async move { stream_thread.run().await });

        Ok(me)
    }

    pub(crate) async fn subscribe(&self) -> Result<NeoInstance> {
        NeoInstance::new(
            self.camera_watch.clone(),
            self.commander.clone(),
            self.cancel.clone(),
        )
    }

    pub(crate) async fn update_config(&self, config: CameraConfig) -> Result<()> {
        self.config_watch.send(config)?;
        Ok(())
    }

    pub(crate) async fn shutdown(&self) {
        let _ = self.commander.send(NeoCamCommand::HangUp).await;
        self.cancel.cancelled().await
    }
}

impl Drop for NeoCam {
    fn drop(&mut self) {
        log::debug!("Cancel:: NeoCam::drop");
        self.cancel.cancel();
    }
}