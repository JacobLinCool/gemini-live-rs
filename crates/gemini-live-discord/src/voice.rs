//! Songbird bridge for owner-only Discord voice and Gemini Live audio.
//!
//! A running bridge owns three coordinated pieces:
//!
//! - a Songbird voice connection joined to the configured Discord channel
//! - a low-latency owner-only receive path into the shared Gemini Live session
//! - a low-latency playback path for Gemini model audio back into Discord
//!
//! The receive side deliberately mirrors the desktop CLI's microphone
//! semantics: while the bot is joined, it continuously streams owner audio
//! into Gemini Live and fills silent ticks with zero PCM. That keeps turn
//! boundary detection on the Gemini side instead of inventing a second VAD
//! policy in Discord land.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::{
    Error as IoError, ErrorKind as IoErrorKind, Read, Result as IoResult, Seek, SeekFrom,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gemini_live_runtime::{GeminiSessionHandle, RuntimeSession};
use serenity::all::{ChannelId, GuildId, UserId};
use songbird::events::TrackEvent;
use songbird::events::context_data::VoiceTick;
use songbird::events::{CoreEvent, Event, EventContext, EventHandler};
use songbird::input::codecs::{get_codec_registry, get_probe};
use songbird::input::core::io::MediaSource;
use songbird::input::{Input, RawAdapter};
use songbird::tracks::TrackHandle;
use songbird::{Call, Songbird};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::DiscordServiceError;

pub type DiscordVoiceManager = Arc<Songbird>;
pub const DISCORD_CAPTURE_SAMPLE_RATE: u32 = 16_000;
pub const MODEL_AUDIO_SAMPLE_RATE: u32 = 24_000;
const DISCORD_CAPTURE_TICK_MS: usize = 20;
const PCM_I16_BYTES_PER_SAMPLE: usize = 2;
const DISCORD_CAPTURE_FRAME_BYTES: usize =
    (DISCORD_CAPTURE_SAMPLE_RATE as usize * DISCORD_CAPTURE_TICK_MS / 1_000)
        * PCM_I16_BYTES_PER_SAMPLE;
const GEMINI_STREAM_CHUNK_MS: usize = 100;
const GEMINI_STREAM_CHUNK_BYTES: usize =
    (DISCORD_CAPTURE_SAMPLE_RATE as usize * GEMINI_STREAM_CHUNK_MS / 1_000)
        * PCM_I16_BYTES_PER_SAMPLE;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceSessionPlan {
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub owner_user_id: UserId,
}

pub struct VoiceBridge {
    manager: DiscordVoiceManager,
    plan: VoiceSessionPlan,
}

struct BridgeShared {
    owner_user_id: UserId,
    owner_audio_tx: mpsc::UnboundedSender<Vec<u8>>,
    ssrc_to_user: Mutex<HashMap<u32, UserId>>,
}

struct SpeakingStateHandler {
    shared: Arc<BridgeShared>,
}

struct VoiceTickHandler {
    shared: Arc<BridgeShared>,
}

struct PlaybackTrackEventLogger {
    event: TrackEvent,
}

struct PlaybackSource {
    shared: Arc<PlaybackShared>,
}

struct PlaybackShared {
    state: Mutex<PlaybackState>,
}

#[derive(Default)]
struct PlaybackState {
    queued: VecDeque<Vec<u8>>,
    queued_offset: usize,
    active: bool,
}

pub struct ActiveVoiceBridge {
    manager: DiscordVoiceManager,
    plan: VoiceSessionPlan,
    session: GeminiSessionHandle,
    playback: Arc<PlaybackShared>,
    playback_track: TrackHandle,
    owner_audio_task: JoinHandle<()>,
}

impl VoiceBridge {
    pub fn new(manager: DiscordVoiceManager, plan: VoiceSessionPlan) -> Self {
        Self { manager, plan }
    }

    pub fn manager(&self) -> &DiscordVoiceManager {
        &self.manager
    }

    pub fn plan(&self) -> &VoiceSessionPlan {
        &self.plan
    }

    pub async fn attach(
        self,
        session: GeminiSessionHandle,
    ) -> Result<ActiveVoiceBridge, DiscordServiceError> {
        let call = self
            .manager
            .join(self.plan.guild_id, self.plan.channel_id)
            .await?;
        let (owner_audio_tx, owner_audio_rx) = mpsc::unbounded_channel();
        let playback = Arc::new(PlaybackShared {
            state: Mutex::new(PlaybackState {
                active: true,
                ..Default::default()
            }),
        });
        let shared = Arc::new(BridgeShared {
            owner_user_id: self.plan.owner_user_id,
            owner_audio_tx,
            ssrc_to_user: Mutex::new(HashMap::new()),
        });

        let playback_track =
            configure_call(&call, Arc::clone(&shared), Arc::clone(&playback)).await?;
        spawn_playback_track_observer(playback_track.clone());

        let owner_audio_task = spawn_owner_audio_forwarder(session.clone(), owner_audio_rx);

        Ok(ActiveVoiceBridge {
            manager: self.manager,
            plan: self.plan,
            session,
            playback,
            playback_track,
            owner_audio_task,
        })
    }
}

impl ActiveVoiceBridge {
    pub fn plan(&self) -> &VoiceSessionPlan {
        &self.plan
    }

    pub fn clear_model_audio(&self) {
        clear_playback_queue(&self.playback);
    }

    pub fn push_model_audio(&self, pcm_i16_le_24k: Vec<u8>) -> Result<(), DiscordServiceError> {
        let pcm_f32_le = pcm_i16le_to_f32le_bytes(&pcm_i16_le_24k);
        let mut state = self.playback.state.lock().expect("playback state lock");
        if !state.active {
            return Err(DiscordServiceError::VoicePlaybackClosed);
        }
        state.queued.push_back(pcm_f32_le);
        Ok(())
    }

    pub async fn shutdown(self) -> Result<(), DiscordServiceError> {
        let Self {
            manager,
            plan,
            session,
            playback,
            playback_track,
            owner_audio_task,
        } = self;
        clear_playback_queue(&playback);
        playback.state.lock().expect("playback state lock").active = false;
        let _ = playback_track.stop();
        owner_audio_task.abort();
        let _ = session.audio_stream_end().await;
        manager.remove(plan.guild_id).await?;
        Ok(())
    }
}

#[serenity::async_trait]
impl EventHandler for SpeakingStateHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::SpeakingStateUpdate(speaking) = ctx
            && let Some(user_id) = speaking.user_id
        {
            self.shared
                .ssrc_to_user
                .lock()
                .expect("ssrc map lock")
                .insert(speaking.ssrc, UserId::new(user_id.0));
        }
        None
    }
}

#[serenity::async_trait]
impl EventHandler for VoiceTickHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        let EventContext::VoiceTick(tick) = ctx else {
            return None;
        };

        let ssrc_to_user = self.shared.ssrc_to_user.lock().expect("ssrc map lock");
        if let Some(pcm_i16_le) = owner_tick_pcm(self.shared.owner_user_id, &ssrc_to_user, tick) {
            let _ = self.shared.owner_audio_tx.send(pcm_i16_le);
        }
        None
    }
}

impl PlaybackSource {
    fn new(shared: Arc<PlaybackShared>) -> Self {
        Self { shared }
    }
}

#[serenity::async_trait]
impl EventHandler for PlaybackTrackEventLogger {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::Track(states) = ctx {
            for (state, _) in *states {
                tracing::info!(
                    "Discord playback track event {:?}: mode={:?}, ready={:?}, position={:?}, play_time={:?}",
                    self.event,
                    state.playing,
                    state.ready,
                    state.position,
                    state.play_time
                );
            }
        }
        None
    }
}

impl Drop for PlaybackSource {
    fn drop(&mut self) {
        tracing::debug!("Discord playback source dropped");
        clear_playback_queue(&self.shared);
        self.shared
            .state
            .lock()
            .expect("playback state lock")
            .active = false;
    }
}

fn clear_playback_queue(playback: &PlaybackShared) {
    let mut state = playback.state.lock().expect("playback state lock");
    state.queued.clear();
    state.queued_offset = 0;
}

impl Read for PlaybackSource {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        let mut written = 0;
        let mut state = self.shared.state.lock().expect("playback state lock");

        while written < buf.len() {
            let (to_copy, consumed_chunk) = match state.queued.front() {
                Some(front) => {
                    let remaining = &front[state.queued_offset..];
                    let to_copy = remaining.len().min(buf.len() - written);
                    buf[written..written + to_copy].copy_from_slice(&remaining[..to_copy]);
                    (to_copy, state.queued_offset + to_copy >= front.len())
                }
                None => {
                    buf[written..].fill(0);
                    written = buf.len();
                    break;
                }
            };
            written += to_copy;
            state.queued_offset += to_copy;
            if consumed_chunk {
                state.queued.pop_front();
                state.queued_offset = 0;
            }
        }

        Ok(written)
    }
}

impl Seek for PlaybackSource {
    fn seek(&mut self, _pos: SeekFrom) -> IoResult<u64> {
        Err(IoError::new(
            IoErrorKind::Unsupported,
            "live playback source is not seekable",
        ))
    }
}

impl MediaSource for PlaybackSource {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

async fn configure_call(
    call: &Arc<tokio::sync::Mutex<Call>>,
    shared: Arc<BridgeShared>,
    playback: Arc<PlaybackShared>,
) -> Result<TrackHandle, DiscordServiceError> {
    let mut call = call.lock().await;
    call.add_global_event(
        Event::Core(CoreEvent::SpeakingStateUpdate),
        SpeakingStateHandler {
            shared: Arc::clone(&shared),
        },
    );
    call.add_global_event(
        Event::Core(CoreEvent::VoiceTick),
        VoiceTickHandler { shared },
    );

    let input = build_ready_live_pcm_input(playback).await?;
    let playback_track = call.play_only_input(input);
    install_playback_track_logging(&playback_track);
    Ok(playback_track)
}

fn build_live_pcm_input(stream: impl MediaSource + 'static, sample_rate: u32) -> Input {
    RawAdapter::new(stream, sample_rate, 1).into()
}

async fn build_ready_live_pcm_input(
    playback: Arc<PlaybackShared>,
) -> Result<Input, DiscordServiceError> {
    // Pre-parse the RawAdapter input up front so the Discord call never sees a
    // background "preparing" track that can fail later with a missing decoder.
    build_live_pcm_input(PlaybackSource::new(playback), MODEL_AUDIO_SAMPLE_RATE)
        .make_playable_async(get_codec_registry(), get_probe())
        .await
        .map_err(Into::into)
}

fn install_playback_track_logging(playback_track: &TrackHandle) {
    for event in [
        TrackEvent::Preparing,
        TrackEvent::Playable,
        TrackEvent::Error,
        TrackEvent::End,
    ] {
        if let Err(error) =
            playback_track.add_event(Event::Track(event), PlaybackTrackEventLogger { event })
        {
            tracing::warn!(
                "failed to attach playback track logger for {:?}: {error}",
                event
            );
        }
    }
}

fn spawn_playback_track_observer(playback_track: TrackHandle) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        match playback_track.get_info().await {
            Ok(state) => tracing::info!(
                "Discord playback track state: mode={:?}, ready={:?}",
                state.playing,
                state.ready
            ),
            Err(error) => tracing::warn!("failed to query playback track state: {error}"),
        }
    });
}

fn spawn_owner_audio_forwarder(
    session: GeminiSessionHandle,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buffered_pcm = Vec::with_capacity(GEMINI_STREAM_CHUNK_BYTES);

        while let Some(frame) = rx.recv().await {
            buffered_pcm.extend_from_slice(&frame);

            while let Some(chunk) = take_ready_audio_chunk(&mut buffered_pcm) {
                if let Err(error) = session
                    .send_audio_at_rate(&chunk, DISCORD_CAPTURE_SAMPLE_RATE)
                    .await
                {
                    tracing::warn!("failed to forward owner audio into Gemini Live: {error}");
                    let _ = session.audio_stream_end().await;
                    return;
                }
            }
        }

        if !buffered_pcm.is_empty()
            && let Err(error) = session
                .send_audio_at_rate(&buffered_pcm, DISCORD_CAPTURE_SAMPLE_RATE)
                .await
        {
            tracing::warn!("failed to flush owner audio into Gemini Live: {error}");
        }
        let _ = session.audio_stream_end().await;
    })
}

fn owner_tick_pcm(
    owner_user_id: UserId,
    ssrc_to_user: &HashMap<u32, UserId>,
    tick: &VoiceTick,
) -> Option<Vec<u8>> {
    owner_tick_pcm_from_parts(
        owner_user_id,
        ssrc_to_user,
        tick.speaking
            .iter()
            .map(|(ssrc, voice_data)| (*ssrc, voice_data.decoded_voice.as_deref())),
        tick.silent.iter().copied(),
    )
}

fn owner_tick_pcm_from_parts<'a>(
    owner_user_id: UserId,
    ssrc_to_user: &HashMap<u32, UserId>,
    speaking: impl IntoIterator<Item = (u32, Option<&'a [i16]>)>,
    silent: impl IntoIterator<Item = u32>,
) -> Option<Vec<u8>> {
    for (ssrc, decoded_voice) in speaking {
        if ssrc_to_user.get(&ssrc) != Some(&owner_user_id) {
            continue;
        }

        let pcm_i16_le = decoded_voice
            .map(decoded_voice_to_pcm_i16le)
            .filter(|pcm| !pcm.is_empty())
            .unwrap_or_else(silence_pcm_frame);
        return Some(pcm_i16_le);
    }

    if silent
        .into_iter()
        .any(|ssrc| ssrc_to_user.get(&ssrc) == Some(&owner_user_id))
    {
        return Some(silence_pcm_frame());
    }

    None
}

fn decoded_voice_to_pcm_i16le(decoded_voice: &[i16]) -> Vec<u8> {
    let mut pcm_i16_le = Vec::with_capacity(decoded_voice.len() * 2);
    for sample in decoded_voice {
        pcm_i16_le.extend_from_slice(&sample.to_le_bytes());
    }
    pcm_i16_le
}

fn silence_pcm_frame() -> Vec<u8> {
    vec![0; DISCORD_CAPTURE_FRAME_BYTES]
}

fn take_ready_audio_chunk(buffered_pcm: &mut Vec<u8>) -> Option<Vec<u8>> {
    if buffered_pcm.len() < GEMINI_STREAM_CHUNK_BYTES {
        return None;
    }

    if buffered_pcm.len() == GEMINI_STREAM_CHUNK_BYTES {
        return Some(std::mem::take(buffered_pcm));
    }

    let remainder = buffered_pcm.split_off(GEMINI_STREAM_CHUNK_BYTES);
    Some(std::mem::replace(buffered_pcm, remainder))
}

fn pcm_i16le_to_f32le_bytes(pcm_i16_le: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity((pcm_i16_le.len() / 2) * 4);
    for sample in pcm_i16_le.chunks_exact(2) {
        let value = i16::from_le_bytes([sample[0], sample[1]]) as f32 / i16::MAX as f32;
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serenity::all::{ChannelId, GuildId, UserId};
    use songbird::input::codecs::{get_codec_registry, get_probe};

    use super::*;

    #[test]
    fn converts_model_audio_to_f32_le() {
        let bytes = pcm_i16le_to_f32le_bytes(&[0x00, 0x00, 0xff, 0x7f]);
        assert_eq!(bytes.len(), 8);
        let first = f32::from_le_bytes(bytes[0..4].try_into().expect("first sample"));
        let second = f32::from_le_bytes(bytes[4..8].try_into().expect("second sample"));
        assert_eq!(first, 0.0);
        assert!(second > 0.99);
    }

    #[test]
    fn voice_session_plan_is_stable() {
        let plan = VoiceSessionPlan {
            guild_id: GuildId::new(1),
            channel_id: ChannelId::new(2),
            owner_user_id: UserId::new(3),
        };

        assert_eq!(plan.guild_id, GuildId::new(1));
        assert_eq!(plan.channel_id, ChannelId::new(2));
        assert_eq!(plan.owner_user_id, UserId::new(3));
    }

    #[tokio::test]
    async fn playback_source_can_be_made_playable_and_stream_multiple_packets() {
        let playback = Arc::new(PlaybackShared {
            state: Mutex::new(PlaybackState {
                active: true,
                ..Default::default()
            }),
        });
        let mut input =
            build_live_pcm_input(PlaybackSource::new(playback), MODEL_AUDIO_SAMPLE_RATE)
                .make_playable_async(get_codec_registry(), get_probe())
                .await
                .expect("live playback input should parse");

        assert!(input.is_playable());

        let parsed = input
            .live_mut()
            .and_then(|live| live.parsed_mut())
            .expect("parsed live input");
        for _ in 0..3 {
            let packet = parsed
                .format
                .next_packet()
                .expect("live playback source should not end");
            assert!(!packet.buf().is_empty());
        }
    }

    #[test]
    fn owner_tick_pcm_uses_owner_voice_when_present() {
        let owner = UserId::new(7);
        let mut ssrc_to_user = HashMap::new();
        ssrc_to_user.insert(42, owner);
        let decoded = [0, i16::MAX];

        let pcm =
            owner_tick_pcm_from_parts(owner, &ssrc_to_user, [(42, Some(decoded.as_slice()))], [])
                .expect("owner pcm");

        assert_eq!(pcm, vec![0x00, 0x00, 0xff, 0x7f]);
    }

    #[test]
    fn owner_tick_pcm_fills_silence_for_owner_silent_tick() {
        let owner = UserId::new(7);
        let mut ssrc_to_user = HashMap::new();
        ssrc_to_user.insert(42, owner);

        let pcm = owner_tick_pcm_from_parts(owner, &ssrc_to_user, [], [42]).expect("owner silence");

        assert_eq!(pcm.len(), DISCORD_CAPTURE_FRAME_BYTES);
        assert!(pcm.iter().all(|sample| *sample == 0));
    }

    #[test]
    fn audio_chunker_flushes_100ms_gemini_frames() {
        let mut buffered = Vec::new();
        for _ in 0..5 {
            buffered.extend_from_slice(&silence_pcm_frame());
        }

        let chunk = take_ready_audio_chunk(&mut buffered).expect("ready chunk");

        assert_eq!(chunk.len(), GEMINI_STREAM_CHUNK_BYTES);
        assert!(buffered.is_empty());
    }

    #[test]
    fn clear_playback_queue_resets_buffered_audio() {
        let playback = PlaybackShared {
            state: Mutex::new(PlaybackState {
                queued: VecDeque::from([vec![1, 2, 3], vec![4, 5]]),
                queued_offset: 2,
                active: true,
            }),
        };

        clear_playback_queue(&playback);

        let state = playback.state.lock().expect("playback state lock");
        assert!(state.queued.is_empty());
        assert_eq!(state.queued_offset, 0);
        assert!(state.active);
    }
}
