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
#[cfg(any(feature = "mic", feature = "share-screen"))]
use gemini_live_runtime::{RuntimeSession, WakeReason};
#[cfg(any(feature = "mic", feature = "share-screen"))]
use tokio::sync::mpsc;

#[cfg(any(feature = "mic", feature = "speak"))]
use crate::app::ServerEventEffect;
#[cfg(any(feature = "mic", feature = "speak", feature = "share-screen"))]
use crate::app::{App, AppCommand};
#[cfg(any(feature = "mic", feature = "speak", feature = "share-screen"))]
use crate::desktop_control::{
    ActiveScreenShare, DesktopControlAction, DesktopControlError, DesktopControlRequest,
    DesktopControlResult, DesktopState, ScreenTargetInfo,
};
#[cfg(any(feature = "mic", feature = "speak", feature = "share-screen"))]
use crate::profile;
#[cfg(any(feature = "mic", feature = "speak", feature = "share-screen"))]
use crate::session::{CliSessionManager, audio_stream_end_if_hot, ensure_hot_session};
#[cfg(any(feature = "mic", feature = "speak", feature = "share-screen"))]
use crate::startup::StartupConfig;

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
        session_manager: &CliSessionManager,
    ) -> io::Result<()> {
        #[cfg(not(feature = "mic"))]
        let _ = session_manager;

        #[cfg(feature = "speak")]
        if startup.speak_enabled {
            let _ = self.set_speaker_enabled(true, app);
            persist_audio_state(store, app)?;
        }

        #[cfg(feature = "mic")]
        if startup.mic_enabled {
            let _ = self.set_mic_enabled(true, app, session_manager).await;
            persist_audio_state(store, app)?;
        }

        Ok(())
    }

    pub(crate) async fn handle_command(
        &mut self,
        command: &AppCommand,
        app: &mut App,
        store: &mut profile::ProfileStore,
        session_manager: &CliSessionManager,
    ) -> io::Result<bool> {
        match command {
            #[cfg(feature = "mic")]
            AppCommand::ToggleMic => {
                let _ = self
                    .set_mic_enabled(!self.mic_enabled(), app, session_manager)
                    .await;
                persist_audio_state(store, app)?;
                Ok(true)
            }
            #[cfg(feature = "speak")]
            AppCommand::ToggleSpeaker => {
                let _ = self.set_speaker_enabled(!self.speaker_enabled(), app);
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
    pub(crate) async fn forward_captured(
        &self,
        captured: CapturedAudio,
        session_manager: &mut CliSessionManager,
    ) {
        let Ok(session) = ensure_hot_session(session_manager, WakeReason::VoiceActivity).await
        else {
            return;
        };
        let _ = session
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
    fn mic_enabled(&self) -> bool {
        self.mic.is_some()
    }

    #[cfg(feature = "mic")]
    async fn set_mic_enabled(
        &mut self,
        enabled: bool,
        app: &mut App,
        session_manager: &CliSessionManager,
    ) -> Result<bool, DesktopControlError> {
        if self.mic_enabled() == enabled {
            return Ok(false);
        }

        if !enabled {
            self.mic = None;
            app.mic_on = false;
            app.sys("mic off".into());
            let _ = audio_stream_end_if_hot(session_manager).await;
            return Ok(true);
        }

        match MicCapture::start(self.mic_tx.clone(), self.aec.clone()) {
            Ok(mic) => {
                app.sys(format!(
                    "mic on ({}Hz → AEC {}Hz)",
                    mic.input_sample_rate, mic.output_sample_rate
                ));
                app.mic_on = true;
                self.mic = Some(mic);
                Ok(true)
            }
            Err(error) => {
                app.mic_on = false;
                let message = format!("mic failed: {error}");
                app.sys(message.clone());
                Err(DesktopControlError::execution_failed(message))
            }
        }
    }

    #[cfg(feature = "speak")]
    fn speaker_enabled(&self) -> bool {
        self.speaker.is_some()
    }

    #[cfg(feature = "speak")]
    fn set_speaker_enabled(
        &mut self,
        enabled: bool,
        app: &mut App,
    ) -> Result<bool, DesktopControlError> {
        if self.speaker_enabled() == enabled {
            return Ok(false);
        }

        if !enabled {
            self.speaker = None;
            app.speak_on = false;
            app.sys("speaker off".into());
            return Ok(true);
        }

        match SpeakerPlayback::start(self.aec.clone()) {
            Ok(speaker) => {
                app.sys(format!(
                    "speaker on ({}Hz, AEC enabled)",
                    speaker.device_sample_rate
                ));
                app.speak_on = true;
                self.speaker = Some(speaker);
                Ok(true)
            }
            Err(error) => {
                app.speak_on = false;
                let message = format!("speaker failed: {error}");
                app.sys(message.clone());
                Err(DesktopControlError::execution_failed(message))
            }
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
#[derive(Debug, Clone)]
struct ScreenShareState {
    target_id: usize,
    interval_secs: f64,
    target_name: String,
}

#[cfg(feature = "share-screen")]
pub(crate) struct DesktopScreen {
    screen_tx: mpsc::Sender<EncodedFrame>,
    screen_rx: mpsc::Receiver<EncodedFrame>,
    share: Option<ScreenCapture>,
    state: Option<ScreenShareState>,
}

#[cfg(feature = "share-screen")]
impl DesktopScreen {
    pub(crate) fn new() -> Self {
        let (screen_tx, screen_rx) = mpsc::channel::<EncodedFrame>(2);
        Self {
            screen_tx,
            screen_rx,
            share: None,
            state: None,
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
                let _ = self.set_screen_share(
                    crate::desktop_control::ScreenShareRequest {
                        enabled: true,
                        target_id: Some(target_id),
                        interval_secs: startup.screen_share.interval_secs,
                    },
                    app,
                );
                persist_screen_state(store, app, self)?;
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
                self.handle_share_args(app, args);
                persist_screen_state(store, app, self)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    pub(crate) async fn next_frame(&mut self) -> Option<EncodedFrame> {
        self.screen_rx.recv().await
    }

    pub(crate) async fn forward_frame(
        &self,
        frame: EncodedFrame,
        session_manager: &mut CliSessionManager,
    ) {
        let Ok(session) = ensure_hot_session(session_manager, WakeReason::ExplicitRefresh).await
        else {
            return;
        };
        let _ = session.send_video(&frame.bytes, frame.mime_type).await;
    }

    fn handle_share_args(&mut self, app: &mut App, args: &str) {
        if args == "list" {
            match self.list_screen_targets() {
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
            return;
        }

        if args.is_empty() {
            if self.share_enabled() {
                let _ = self.set_screen_share(
                    crate::desktop_control::ScreenShareRequest {
                        enabled: false,
                        target_id: None,
                        interval_secs: None,
                    },
                    app,
                );
            } else {
                app.sys("usage: /share-screen list | /share-screen <id> [interval_secs]".into());
            }
            return;
        }

        let mut parts = args.split_whitespace();
        let id = match parts.next().and_then(|value| value.parse().ok()) {
            Some(id) => id,
            None => {
                app.sys("invalid id — use /share-screen list".into());
                return;
            }
        };
        let interval_secs = parts
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1.0);
        let _ = self.set_screen_share(
            crate::desktop_control::ScreenShareRequest {
                enabled: true,
                target_id: Some(id),
                interval_secs: Some(interval_secs),
            },
            app,
        );
    }

    fn share_enabled(&self) -> bool {
        self.share.is_some()
    }

    fn current_share(&self) -> Option<ActiveScreenShare> {
        self.state.as_ref().map(|state| ActiveScreenShare {
            target_id: state.target_id,
            interval_secs: state.interval_secs,
            target_name: state.target_name.clone(),
        })
    }

    fn list_screen_targets(&self) -> Result<Vec<ScreenTargetInfo>, DesktopControlError> {
        list_targets()
            .map(|targets| {
                targets
                    .into_iter()
                    .map(|target| ScreenTargetInfo {
                        id: target.id,
                        kind: target.kind.to_string(),
                        name: target.name,
                        width: target.width,
                        height: target.height,
                    })
                    .collect()
            })
            .map_err(|error| DesktopControlError::execution_failed(error.to_string()))
    }

    fn set_screen_share(
        &mut self,
        request: crate::desktop_control::ScreenShareRequest,
        app: &mut App,
    ) -> Result<bool, DesktopControlError> {
        if !request.enabled {
            if !self.share_enabled() {
                return Ok(false);
            }
            self.share = None;
            self.state = None;
            app.screen_on = false;
            app.sys("screen share stopped".into());
            return Ok(true);
        }

        let target_id = request.target_id.ok_or_else(|| {
            DesktopControlError::invalid("`targetId` is required when enabling screen share")
        })?;
        let interval_secs = request.interval_secs.unwrap_or(1.0);
        if interval_secs <= 0.0 {
            return Err(DesktopControlError::invalid(
                "`intervalSecs` must be greater than 0",
            ));
        }
        let unchanged = self.state.as_ref().is_some_and(|state| {
            state.target_id == target_id
                && (state.interval_secs - interval_secs).abs() < f64::EPSILON
        });
        if unchanged && self.share_enabled() {
            return Ok(false);
        }

        self.share = None;
        let config = ScreenCaptureConfig {
            interval: std::time::Duration::from_secs_f64(interval_secs),
            ..ScreenCaptureConfig::default()
        };

        match ScreenCapture::start(target_id, config, self.screen_tx.clone()) {
            Ok(share) => {
                let target_name = share.target.name.clone();
                app.sys(format!(
                    "sharing \"{}\" every {:.1}s",
                    target_name, interval_secs
                ));
                app.screen_on = true;
                self.state = Some(ScreenShareState {
                    target_id,
                    interval_secs,
                    target_name,
                });
                self.share = Some(share);
                Ok(true)
            }
            Err(error) => {
                self.state = None;
                app.screen_on = false;
                let message = format!("screen share failed: {error}");
                app.sys(message.clone());
                Err(DesktopControlError::execution_failed(message))
            }
        }
    }
}

#[cfg(feature = "share-screen")]
fn persist_screen_state(
    store: &mut profile::ProfileStore,
    app: &App,
    screen: &DesktopScreen,
) -> io::Result<()> {
    store.set_screen_share(
        app.screen_on,
        screen.state.as_ref().map(|state| state.target_id),
        screen.state.as_ref().map(|state| state.interval_secs),
    )
}

#[cfg(any(feature = "mic", feature = "speak", feature = "share-screen"))]
pub(crate) async fn handle_control_request(
    request: DesktopControlRequest,
    app: &mut App,
    store: &mut profile::ProfileStore,
    session_manager: &CliSessionManager,
    #[cfg(any(feature = "mic", feature = "speak"))] audio: &mut DesktopAudio,
    #[cfg(feature = "share-screen")] screen: &mut DesktopScreen,
) {
    let action = request.action().clone();
    let result = match action {
        DesktopControlAction::GetState => Ok(DesktopControlResult::State {
            state: current_desktop_state(
                #[cfg(any(feature = "mic", feature = "speak"))]
                audio,
                #[cfg(feature = "share-screen")]
                screen,
            ),
        }),
        #[cfg(feature = "mic")]
        DesktopControlAction::SetMicrophone { enabled } => {
            let result = audio.set_mic_enabled(enabled, app, session_manager).await;
            if result.is_ok()
                && let Err(error) = persist_audio_state(store, app)
            {
                request.respond(Err(DesktopControlError::execution_failed(format!(
                    "failed to persist audio state: {error}"
                ))));
                return;
            }
            result.map(|changed| DesktopControlResult::StateChange {
                changed,
                state: current_desktop_state(
                    #[cfg(any(feature = "mic", feature = "speak"))]
                    audio,
                    #[cfg(feature = "share-screen")]
                    screen,
                ),
            })
        }
        #[cfg(feature = "speak")]
        DesktopControlAction::SetSpeaker { enabled } => {
            let result = audio.set_speaker_enabled(enabled, app);
            if result.is_ok()
                && let Err(error) = persist_audio_state(store, app)
            {
                request.respond(Err(DesktopControlError::execution_failed(format!(
                    "failed to persist audio state: {error}"
                ))));
                return;
            }
            result.map(|changed| DesktopControlResult::StateChange {
                changed,
                state: current_desktop_state(
                    #[cfg(any(feature = "mic", feature = "speak"))]
                    audio,
                    #[cfg(feature = "share-screen")]
                    screen,
                ),
            })
        }
        #[cfg(feature = "share-screen")]
        DesktopControlAction::ListScreenTargets => screen
            .list_screen_targets()
            .map(|targets| DesktopControlResult::ScreenTargets { targets }),
        #[cfg(feature = "share-screen")]
        DesktopControlAction::SetScreenShare(screen_request) => {
            let result = screen.set_screen_share(screen_request, app);
            if result.is_ok()
                && let Err(error) = persist_screen_state(store, app, screen)
            {
                request.respond(Err(DesktopControlError::execution_failed(format!(
                    "failed to persist screen-share state: {error}"
                ))));
                return;
            }
            result.map(|changed| DesktopControlResult::StateChange {
                changed,
                state: current_desktop_state(
                    #[cfg(any(feature = "mic", feature = "speak"))]
                    audio,
                    #[cfg(feature = "share-screen")]
                    screen,
                ),
            })
        }
    };

    request.respond(result);
}

#[cfg(any(feature = "mic", feature = "speak", feature = "share-screen"))]
fn current_desktop_state(
    #[cfg(any(feature = "mic", feature = "speak"))] audio: &DesktopAudio,
    #[cfg(feature = "share-screen")] screen: &DesktopScreen,
) -> DesktopState {
    DesktopState {
        #[cfg(feature = "mic")]
        microphone_enabled: audio.mic_enabled(),
        #[cfg(feature = "speak")]
        speaker_enabled: audio.speaker_enabled(),
        #[cfg(feature = "share-screen")]
        screen_share: screen.current_share(),
    }
}
