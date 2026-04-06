//! Desktop host wiring on top of `gemini-live-io`.
//!
//! This module is the CLI-side integration layer that turns reusable desktop
//! media adapters into product behavior: auto-start from profiles, slash
//! command toggles, persistence write-back, and forwarding media into the
//! managed runtime.

#[cfg(any(feature = "mic", feature = "speak", feature = "share-screen"))]
use std::io;

#[cfg(any(feature = "mic", feature = "speak"))]
use gemini_live_io::audio::AecHandle;
#[cfg(feature = "speak")]
use gemini_live_io::audio::SpeakerPlayback;
#[cfg(feature = "mic")]
use gemini_live_io::audio::{CapturedAudio, MicCapture};
#[cfg(feature = "share-screen")]
use gemini_live_io::screen::{EncodedFrame, ScreenCapture, ScreenCaptureConfig, list_targets};
#[cfg(any(feature = "mic", feature = "speak", feature = "share-screen"))]
use gemini_live_runtime::{GeminiSessionDriver, ManagedRuntime};
#[cfg(any(feature = "mic", feature = "share-screen"))]
use tokio::sync::mpsc;

#[cfg(any(feature = "mic", feature = "speak"))]
use crate::app::ServerEventEffect;
#[cfg(any(feature = "mic", feature = "speak", feature = "share-screen"))]
use crate::app::{App, AppCommand};
#[cfg(any(feature = "mic", feature = "speak", feature = "share-screen"))]
use crate::profile;
#[cfg(any(feature = "mic", feature = "speak", feature = "share-screen"))]
use crate::startup::StartupConfig;
#[cfg(any(feature = "mic", feature = "speak", feature = "share-screen"))]
use crate::tooling;

#[cfg(any(feature = "mic", feature = "speak", feature = "share-screen"))]
type CliRuntime = ManagedRuntime<GeminiSessionDriver, tooling::ToolRuntime>;

#[cfg(any(feature = "mic", feature = "speak"))]
pub(crate) struct DesktopAudio {
    aec: AecHandle,
    #[cfg(feature = "mic")]
    mic_tx: mpsc::Sender<CapturedAudio>,
    #[cfg(feature = "mic")]
    mic_rx: mpsc::Receiver<CapturedAudio>,
    #[cfg(feature = "mic")]
    mic: Option<MicCapture>,
    #[cfg(feature = "speak")]
    speaker: Option<SpeakerPlayback>,
}

#[cfg(any(feature = "mic", feature = "speak"))]
impl DesktopAudio {
    pub(crate) fn new() -> Result<Self, gemini_live_io::error::AudioIoError> {
        #[cfg(feature = "mic")]
        let (mic_tx, mic_rx) = mpsc::channel::<CapturedAudio>(32);

        Ok(Self {
            aec: AecHandle::new()?,
            #[cfg(feature = "mic")]
            mic_tx,
            #[cfg(feature = "mic")]
            mic_rx,
            #[cfg(feature = "mic")]
            mic: None,
            #[cfg(feature = "speak")]
            speaker: None,
        })
    }

    pub(crate) async fn autostart(
        &mut self,
        startup: &StartupConfig,
        app: &mut App,
        store: &mut profile::ProfileStore,
        runtime: &CliRuntime,
    ) -> io::Result<()> {
        #[cfg(not(feature = "mic"))]
        let _ = runtime;

        #[cfg(feature = "speak")]
        if startup.speak_enabled {
            self.toggle_speaker(app);
            persist_audio_state(store, app)?;
        }

        #[cfg(feature = "mic")]
        if startup.mic_enabled {
            self.toggle_mic(app, runtime).await;
            persist_audio_state(store, app)?;
        }

        Ok(())
    }

    pub(crate) async fn handle_command(
        &mut self,
        command: &AppCommand,
        app: &mut App,
        store: &mut profile::ProfileStore,
        runtime: &CliRuntime,
    ) -> io::Result<bool> {
        match command {
            #[cfg(feature = "mic")]
            AppCommand::ToggleMic => {
                self.toggle_mic(app, runtime).await;
                persist_audio_state(store, app)?;
                Ok(true)
            }
            #[cfg(feature = "speak")]
            AppCommand::ToggleSpeaker => {
                self.toggle_speaker(app);
                persist_audio_state(store, app)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    #[cfg(feature = "mic")]
    pub(crate) async fn next_captured(&mut self) -> Option<CapturedAudio> {
        self.mic_rx.recv().await
    }

    #[cfg(feature = "mic")]
    pub(crate) async fn forward_captured(&self, captured: CapturedAudio, runtime: &CliRuntime) {
        let _ = runtime
            .send_audio_at_rate(&captured.pcm_i16_le, captured.sample_rate)
            .await;
    }

    pub(crate) fn apply_server_effect(&mut self, effect: ServerEventEffect) {
        match effect {
            #[cfg(feature = "speak")]
            ServerEventEffect::PlayAudio(data) => {
                if let Some(speaker) = &self.speaker {
                    speaker.push_model_pcm24k_i16(&data);
                }
            }
            #[cfg(feature = "speak")]
            ServerEventEffect::ClearAudio => {
                if let Some(speaker) = &self.speaker {
                    speaker.clear();
                }
            }
            ServerEventEffect::None => {}
        }
    }

    #[cfg(feature = "mic")]
    async fn toggle_mic(&mut self, app: &mut App, runtime: &CliRuntime) {
        if self.mic.is_some() {
            self.mic = None;
            app.mic_on = false;
            app.sys("mic off".into());
            let _ = runtime.audio_stream_end().await;
            return;
        }

        match MicCapture::start(self.mic_tx.clone(), self.aec.clone()) {
            Ok(mic) => {
                app.sys(format!(
                    "mic on ({}Hz → AEC {}Hz)",
                    mic.input_sample_rate, mic.output_sample_rate
                ));
                app.mic_on = true;
                self.mic = Some(mic);
            }
            Err(error) => app.sys(format!("mic failed: {error}")),
        }
    }

    #[cfg(feature = "speak")]
    fn toggle_speaker(&mut self, app: &mut App) {
        if self.speaker.is_some() {
            self.speaker = None;
            app.speak_on = false;
            app.sys("speaker off".into());
            return;
        }

        match SpeakerPlayback::start(self.aec.clone()) {
            Ok(speaker) => {
                app.sys(format!(
                    "speaker on ({}Hz, AEC enabled)",
                    speaker.device_sample_rate
                ));
                app.speak_on = true;
                self.speaker = Some(speaker);
            }
            Err(error) => app.sys(format!("speaker failed: {error}")),
        }
    }
}

#[cfg(any(feature = "mic", feature = "speak"))]
fn persist_audio_state(store: &mut profile::ProfileStore, app: &App) -> io::Result<()> {
    store.set_audio_state(
        {
            #[cfg(feature = "mic")]
            {
                app.mic_on
            }
            #[cfg(not(feature = "mic"))]
            {
                false
            }
        },
        {
            #[cfg(feature = "speak")]
            {
                app.speak_on
            }
            #[cfg(not(feature = "speak"))]
            {
                false
            }
        },
    )
}

#[cfg(feature = "share-screen")]
#[derive(Debug, Clone, Copy, Default)]
struct ScreenShareState {
    target_id: Option<usize>,
    interval_secs: Option<f64>,
}

#[cfg(feature = "share-screen")]
pub(crate) struct DesktopScreen {
    screen_tx: mpsc::Sender<EncodedFrame>,
    screen_rx: mpsc::Receiver<EncodedFrame>,
    share: Option<ScreenCapture>,
}

#[cfg(feature = "share-screen")]
impl DesktopScreen {
    pub(crate) fn new() -> Self {
        let (screen_tx, screen_rx) = mpsc::channel::<EncodedFrame>(2);
        Self {
            screen_tx,
            screen_rx,
            share: None,
        }
    }

    pub(crate) fn autostart(
        &mut self,
        startup: &StartupConfig,
        app: &mut App,
        store: &mut profile::ProfileStore,
    ) -> io::Result<()> {
        if startup.screen_share.enabled == Some(true) {
            if let Some(target_id) = startup.screen_share.target_id {
                let interval = startup.screen_share.interval_secs.unwrap_or(1.0);
                let args = format!("{target_id} {interval}");
                let state = self.handle_share_args(app, &args);
                persist_screen_state(store, app, state)?;
            } else {
                app.sys(
                    "screen share profile requested auto-start, but no target id is configured"
                        .into(),
                );
                store.set_screen_share(false, None, startup.screen_share.interval_secs)?;
            }
        }

        Ok(())
    }

    pub(crate) fn handle_command(
        &mut self,
        command: &AppCommand,
        app: &mut App,
        store: &mut profile::ProfileStore,
    ) -> io::Result<bool> {
        match command {
            AppCommand::ShareScreen(args) => {
                let state = self.handle_share_args(app, args);
                persist_screen_state(store, app, state)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    pub(crate) async fn next_frame(&mut self) -> Option<EncodedFrame> {
        self.screen_rx.recv().await
    }

    pub(crate) async fn forward_frame(&self, frame: EncodedFrame, runtime: &CliRuntime) {
        let _ = runtime.send_video(&frame.bytes, frame.mime_type).await;
    }

    fn handle_share_args(&mut self, app: &mut App, args: &str) -> ScreenShareState {
        if args == "list" {
            match list_targets() {
                Ok(targets) if targets.is_empty() => app.sys("no capture targets found".into()),
                Ok(targets) => {
                    for target in &targets {
                        app.sys(format!(
                            "  {}: [{}] {} ({}x{})",
                            target.id, target.kind, target.name, target.width, target.height
                        ));
                    }
                }
                Err(error) => app.sys(format!("failed to enumerate capture targets: {error}")),
            }
            return ScreenShareState::default();
        }

        if args.is_empty() {
            if self.share.is_some() {
                self.share = None;
                app.screen_on = false;
                app.sys("screen share stopped".into());
            } else {
                app.sys("usage: /share-screen list | /share-screen <id> [interval_secs]".into());
            }
            return ScreenShareState::default();
        }

        let mut parts = args.split_whitespace();
        let id = match parts.next().and_then(|value| value.parse().ok()) {
            Some(id) => id,
            None => {
                app.sys("invalid id — use /share-screen list".into());
                return ScreenShareState::default();
            }
        };
        let interval_secs = parts
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1.0);

        self.share = None;
        let config = ScreenCaptureConfig {
            interval: std::time::Duration::from_secs_f64(interval_secs),
            ..ScreenCaptureConfig::default()
        };

        match ScreenCapture::start(id, config, self.screen_tx.clone()) {
            Ok(share) => {
                app.sys(format!(
                    "sharing \"{}\" every {:.1}s",
                    share.target.name, interval_secs
                ));
                app.screen_on = true;
                self.share = Some(share);
            }
            Err(error) => {
                app.screen_on = false;
                app.sys(format!("screen share failed: {error}"));
            }
        }

        ScreenShareState {
            target_id: Some(id),
            interval_secs: Some(interval_secs),
        }
    }
}

#[cfg(feature = "share-screen")]
fn persist_screen_state(
    store: &mut profile::ProfileStore,
    app: &App,
    state: ScreenShareState,
) -> io::Result<()> {
    store.set_screen_share(app.screen_on, state.target_id, state.interval_secs)
}
