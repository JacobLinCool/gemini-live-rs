//! Top-level Discord service and gateway integration.
//!
//! This module owns the single-guild host runtime:
//!
//! - lazily wake one shared Gemini Live session when text or voice needs it
//! - create or reuse the configured Discord voice channel
//! - receive text messages from that channel's chat surface
//! - auto-join voice when the configured owner enters the channel
//! - auto-leave when the owner leaves
//! - project Gemini output back into Discord chat and voice

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gemini_live::ServerEvent;
use gemini_live::types::{Content, Part};
use gemini_live_runtime::{
    ActivityKind, RuntimeEvent, RuntimeEventReceiver, RuntimeLifecycleEvent, RuntimeSendFailure,
    SessionLifecycleState, WakeReason,
};
use serenity::Client;
use serenity::all::{
    ApplicationId, ChannelId, Context, EventHandler, Guild, GuildId, Message, Permissions, Ready,
    Scope,
};
use serenity::async_trait;
use serenity::builder::CreateBotAuthParameters;
use serenity::http::Http;
use songbird::driver::{Channels, DecodeConfig, DecodeMode, SampleRate};
use songbird::serenity::{SerenityInit, get as get_songbird};
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;

use crate::config::DiscordBotConfig;
use crate::error::DiscordServiceError;
use crate::gateway::gateway_intents;
use crate::policy::BotConversationScope;
use crate::session::{DiscordSessionManager, new_session_manager};
use crate::setup::ensure_target_voice_channel;
use crate::voice::{ActiveVoiceBridge, VoiceBridge, VoiceSessionPlan};

/// Mutable service state that survives across Discord gateway events.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DiscordServiceState {
    pub target_channel_id: Option<ChannelId>,
    pub joined_voice_channel_id: Option<ChannelId>,
    pub setup_error: Option<String>,
}

/// Builder for the Discord host service.
#[derive(Debug, Clone)]
pub struct DiscordAgentService {
    config: DiscordBotConfig,
}

impl DiscordAgentService {
    pub fn new(config: DiscordBotConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &DiscordBotConfig {
        &self.config
    }

    pub fn prepare(&self) -> PreparedDiscordService {
        let (session_manager, runtime_events) = new_session_manager(&self.config);
        PreparedDiscordService {
            config: self.config.clone(),
            session_manager,
            runtime_events,
            state: DiscordServiceState::default(),
        }
    }
}

/// Prepared service instance with a configured Gemini Live runtime.
pub struct PreparedDiscordService {
    config: DiscordBotConfig,
    session_manager: DiscordSessionManager,
    runtime_events: RuntimeEventReceiver,
    state: DiscordServiceState,
}

#[derive(Clone)]
struct SharedBotState {
    inner: Arc<SharedBotStateInner>,
}

struct SharedBotStateInner {
    config: DiscordBotConfig,
    session_manager: Mutex<DiscordSessionManager>,
    service_state: RwLock<DiscordServiceState>,
    pending_text_replies: Mutex<VecDeque<PendingTextReply>>,
    voice_bridge: Mutex<Option<ActiveVoiceBridge>>,
}

const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingTextReply {
    channel_id: ChannelId,
    prompt: String,
}

struct RuntimeReplyState {
    active: Option<ChatReplyAccumulator>,
}

struct CompletedTurn {
    channel_id: ChannelId,
    rendered_text: String,
    user_turn: Option<Content>,
    model_turn: Option<Content>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyOrigin {
    TextTurn,
    VoiceTurn,
}

#[derive(Debug)]
struct ChatReplyAccumulator {
    origin: ReplyOrigin,
    channel_id: ChannelId,
    user_input: String,
    output_transcription: String,
    model_text: String,
}

#[derive(Clone)]
struct DiscordGatewayHandler {
    state: SharedBotState,
}

impl PreparedDiscordService {
    pub fn config(&self) -> &DiscordBotConfig {
        &self.config
    }

    pub fn state(&self) -> &DiscordServiceState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut DiscordServiceState {
        &mut self.state
    }

    pub fn session_manager(&mut self) -> &mut DiscordSessionManager {
        &mut self.session_manager
    }

    pub fn runtime_events(&mut self) -> &mut RuntimeEventReceiver {
        &mut self.runtime_events
    }

    pub async fn run(self) -> Result<(), DiscordServiceError> {
        let shared_state = SharedBotState::new(self.config.clone(), self.session_manager);
        let handler = DiscordGatewayHandler {
            state: shared_state.clone(),
        };
        let songbird_config = discord_songbird_config();
        let mut client = Client::builder(&self.config.discord_bot_token, gateway_intents())
            .event_handler(handler)
            .register_songbird_from_config(songbird_config)
            .await?;
        let http = client.http.clone();
        print_invite_link(&http, self.config.guild_id).await;

        if let Err(error) = shared_state.ensure_target_channel(&http).await {
            tracing::warn!("initial Discord setup failed: {error}");
        }

        let runtime_task =
            spawn_runtime_event_loop(shared_state.clone(), self.runtime_events, http);
        let idle_task = spawn_idle_dormancy_loop(shared_state.clone());
        let start_result = client.start().await;
        runtime_task.abort();
        idle_task.abort();
        let close_result = shared_state.close_runtime().await;

        start_result?;
        close_result?;
        Ok(())
    }
}

impl SharedBotState {
    fn new(config: DiscordBotConfig, session_manager: DiscordSessionManager) -> Self {
        Self {
            inner: Arc::new(SharedBotStateInner {
                config,
                session_manager: Mutex::new(session_manager),
                service_state: RwLock::new(DiscordServiceState::default()),
                pending_text_replies: Mutex::new(VecDeque::new()),
                voice_bridge: Mutex::new(None),
            }),
        }
    }

    fn config(&self) -> &DiscordBotConfig {
        &self.inner.config
    }

    async fn target_channel_id(&self) -> Option<ChannelId> {
        self.inner.service_state.read().await.target_channel_id
    }

    async fn ensure_target_channel(
        &self,
        http: &Arc<Http>,
    ) -> Result<ChannelId, DiscordServiceError> {
        match ensure_target_voice_channel(
            http,
            self.inner.config.guild_id,
            &self.inner.config.voice_channel_name,
        )
        .await
        {
            Ok(channel) => {
                let mut state = self.inner.service_state.write().await;
                state.target_channel_id = Some(channel.id);
                state.setup_error = None;
                Ok(channel.id)
            }
            Err(error) => {
                let mut state = self.inner.service_state.write().await;
                state.setup_error = Some(error.to_string());
                Err(error.into())
            }
        }
    }

    async fn maybe_join_if_owner_present(
        &self,
        ctx: &Context,
        guild: &Guild,
    ) -> Result<(), DiscordServiceError> {
        let Some(target_channel_id) = self.target_channel_id().await else {
            return Ok(());
        };
        if guild
            .voice_states
            .get(&self.inner.config.owner_user_id)
            .and_then(|state| state.channel_id)
            == Some(target_channel_id)
        {
            self.join_voice(ctx, target_channel_id).await?;
        }
        Ok(())
    }

    async fn send_discord_message_turn(
        &self,
        message: &Message,
    ) -> Result<(), DiscordServiceError> {
        let prompt = format_user_message_for_model(message);
        self.inner
            .pending_text_replies
            .lock()
            .await
            .push_back(PendingTextReply {
                channel_id: message.channel_id,
                prompt: prompt.clone(),
            });
        let mut session_manager = self.inner.session_manager.lock().await;
        session_manager
            .ensure_hot(WakeReason::TextInput, Instant::now())
            .await?;
        if let Err(error) = session_manager.runtime().send_text(&prompt).await {
            let _ = self.inner.pending_text_replies.lock().await.pop_back();
            return Err(DiscordServiceError::from(error));
        }
        Ok(())
    }

    async fn next_pending_text_reply(&self) -> Option<PendingTextReply> {
        self.inner.pending_text_replies.lock().await.pop_front()
    }

    async fn join_voice(
        &self,
        ctx: &Context,
        target_channel_id: ChannelId,
    ) -> Result<(), DiscordServiceError> {
        let mut bridge_slot = self.inner.voice_bridge.lock().await;
        if bridge_slot
            .as_ref()
            .map(|bridge| bridge.plan().channel_id == target_channel_id)
            .unwrap_or(false)
        {
            return Ok(());
        }
        if let Some(existing) = bridge_slot.take() {
            existing.shutdown().await?;
        }

        let session = {
            let mut session_manager = self.inner.session_manager.lock().await;
            session_manager
                .ensure_hot(WakeReason::VoiceJoin, Instant::now())
                .await?;
            session_manager
                .active_session()
                .ok_or(DiscordServiceError::from(
                    gemini_live_runtime::RuntimeError::NotConnected,
                ))?
        };
        let manager = get_songbird(ctx)
            .await
            .ok_or(DiscordServiceError::SongbirdUnavailable)?;
        let bridge = VoiceBridge::new(
            manager,
            VoiceSessionPlan {
                guild_id: self.inner.config.guild_id,
                channel_id: target_channel_id,
                owner_user_id: self.inner.config.owner_user_id,
            },
        )
        .attach(session)
        .await?;
        *bridge_slot = Some(bridge);

        let mut state = self.inner.service_state.write().await;
        state.joined_voice_channel_id = Some(target_channel_id);
        Ok(())
    }

    async fn leave_voice(&self) -> Result<(), DiscordServiceError> {
        let existing = self.inner.voice_bridge.lock().await.take();
        if let Some(bridge) = existing {
            bridge.shutdown().await?;
        }

        let mut state = self.inner.service_state.write().await;
        state.joined_voice_channel_id = None;
        Ok(())
    }

    async fn push_model_audio(&self, pcm_i16_le_24k: Vec<u8>) -> Result<(), DiscordServiceError> {
        let guard = self.inner.voice_bridge.lock().await;
        if let Some(bridge) = guard.as_ref() {
            bridge.push_model_audio(pcm_i16_le_24k)?;
        }
        Ok(())
    }

    async fn clear_model_audio(&self) {
        let guard = self.inner.voice_bridge.lock().await;
        if let Some(bridge) = guard.as_ref() {
            bridge.clear_model_audio();
        }
    }

    async fn close_runtime(&self) -> Result<(), DiscordServiceError> {
        self.leave_voice().await?;
        let mut session_manager = self.inner.session_manager.lock().await;
        session_manager.enter_dormant().await?;
        Ok(())
    }

    async fn sync_resume_handle_from_runtime(&self, issued_at: Instant) {
        let session_manager = self.inner.session_manager.lock().await;
        session_manager.sync_resume_handle_from_runtime(issued_at);
    }

    async fn record_runtime_activity(&self, kind: ActivityKind, at: Instant) {
        let session_manager = self.inner.session_manager.lock().await;
        session_manager.record_activity(kind, at);
    }

    async fn record_completed_turn(&self, turn: &CompletedTurn) {
        let session_manager = self.inner.session_manager.lock().await;
        if let Some(user_turn) = turn.user_turn.clone() {
            session_manager.record_recent_turn(user_turn);
        }
        if let Some(model_turn) = turn.model_turn.clone() {
            session_manager.record_recent_turn(model_turn);
        }
    }

    async fn maybe_enter_dormant_if_idle(&self, now: Instant) -> Result<(), DiscordServiceError> {
        if self.inner.voice_bridge.lock().await.is_some() {
            return Ok(());
        }

        let mut session_manager = self.inner.session_manager.lock().await;
        if session_manager.lifecycle_state() == SessionLifecycleState::Dormant {
            return Ok(());
        }
        if session_manager.idle_decision(now) == gemini_live_runtime::IdleDecision::EnterDormant {
            session_manager.enter_dormant().await?;
        }
        Ok(())
    }
}

impl RuntimeReplyState {
    fn new() -> Self {
        Self { active: None }
    }

    fn append_input_transcription(&mut self, text: &str) {
        let active = self
            .active
            .as_mut()
            .expect("reply target must be established before input transcription append");
        active.user_input.push_str(text);
    }

    fn append_output_transcription(&mut self, text: &str) {
        let active = self
            .active
            .as_mut()
            .expect("reply target must be established before transcription append");
        active.output_transcription.push_str(text);
    }

    fn append_model_text(&mut self, text: &str) {
        let active = self
            .active
            .as_mut()
            .expect("reply target must be established before model text append");
        active.model_text.push_str(text);
    }

    fn discard_active(&mut self) {
        self.active = None;
    }

    fn finish_turn(&mut self) -> Option<CompletedTurn> {
        self.active.take().map(ChatReplyAccumulator::finish_turn)
    }
}

impl ChatReplyAccumulator {
    fn for_text_turn(channel_id: ChannelId, prompt: String) -> Self {
        Self {
            origin: ReplyOrigin::TextTurn,
            channel_id,
            user_input: prompt,
            output_transcription: String::new(),
            model_text: String::new(),
        }
    }

    fn for_voice_turn(channel_id: ChannelId) -> Self {
        Self {
            origin: ReplyOrigin::VoiceTurn,
            channel_id,
            user_input: String::new(),
            output_transcription: String::new(),
            model_text: String::new(),
        }
    }

    fn finish_turn(self) -> CompletedTurn {
        let rendered_text = self.rendered_text();
        CompletedTurn {
            channel_id: self.channel_id,
            rendered_text: rendered_text.clone(),
            user_turn: content_from_turn_text("user", self.user_input),
            model_turn: content_from_turn_text("model", rendered_text),
        }
    }

    fn rendered_text(&self) -> String {
        let output = self.output_transcription.trim();
        if !output.is_empty() {
            output.to_owned()
        } else if self.is_text_turn() {
            self.model_text.trim().to_owned()
        } else {
            String::new()
        }
    }

    fn is_text_turn(&self) -> bool {
        self.origin == ReplyOrigin::TextTurn
    }
}

#[async_trait]
impl EventHandler for DiscordGatewayHandler {
    async fn ready(&self, ctx: Context, _: Ready) {
        if let Err(error) = self.state.ensure_target_channel(&ctx.http).await {
            tracing::warn!("ready setup failed: {error}");
        }
    }

    async fn guild_create(&self, ctx: Context, guild: Guild, _: Option<bool>) {
        if guild.id != self.state.config().guild_id {
            return;
        }
        match self.state.ensure_target_channel(&ctx.http).await {
            Ok(_) => {
                if let Err(error) = self.state.maybe_join_if_owner_present(&ctx, &guild).await {
                    tracing::warn!(
                        "failed to auto-join owner voice session on guild sync: {error}"
                    );
                }
            }
            Err(error) => tracing::warn!("guild setup failed: {error}"),
        }
    }

    async fn message(&self, ctx: Context, message: Message) {
        let config = self.state.config();
        if message.guild_id != Some(config.guild_id) || message.author.bot {
            return;
        }

        let Some(target_channel_id) = self.state.target_channel_id().await else {
            return;
        };
        let scope = BotConversationScope {
            channel_id: target_channel_id,
            owner_user_id: config.owner_user_id,
        };
        if !scope.accepts_text_message(message.channel_id, message.author.bot) {
            return;
        }

        if let Err(error) = self.state.send_discord_message_turn(&message).await {
            tracing::warn!("failed to forward Discord text message into Gemini Live: {error}");
            if let Err(send_error) = message
                .channel_id
                .say(
                    &ctx.http,
                    format!("failed to send message to Gemini Live: {error}"),
                )
                .await
            {
                tracing::warn!("failed to report send error in Discord chat: {send_error}");
            }
        }
    }

    async fn voice_state_update(
        &self,
        ctx: Context,
        old: Option<serenity::all::VoiceState>,
        new: serenity::all::VoiceState,
    ) {
        if new.guild_id != Some(self.state.config().guild_id) {
            return;
        }
        let Some(target_channel_id) = self.state.target_channel_id().await else {
            return;
        };
        let scope = BotConversationScope {
            channel_id: target_channel_id,
            owner_user_id: self.state.config().owner_user_id,
        };
        let previous_channel = old.and_then(|state| state.channel_id);

        if scope.owner_joined_target_channel(new.user_id, previous_channel, new.channel_id) {
            if let Err(error) = self.state.join_voice(&ctx, target_channel_id).await {
                tracing::warn!("failed to join target voice channel: {error}");
            }
        } else if scope.owner_left_target_channel(new.user_id, previous_channel, new.channel_id)
            && let Err(error) = self.state.leave_voice().await
        {
            tracing::warn!("failed to leave target voice channel: {error}");
        }
    }
}

fn spawn_runtime_event_loop(
    state: SharedBotState,
    mut runtime_events: RuntimeEventReceiver,
    http: Arc<Http>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut reply_state = RuntimeReplyState::new();
        while let Some(event) = runtime_events.recv().await {
            if let Err(error) = handle_runtime_event(&state, &mut reply_state, &http, event).await {
                tracing::warn!("runtime projection error: {error}");
            }
        }
    })
}

async fn handle_runtime_event(
    state: &SharedBotState,
    reply_state: &mut RuntimeReplyState,
    http: &Arc<Http>,
    event: RuntimeEvent,
) -> Result<(), DiscordServiceError> {
    match event {
        RuntimeEvent::Lifecycle(lifecycle) => {
            log_runtime_lifecycle(&lifecycle);
        }
        RuntimeEvent::SendFailed(failure) => {
            log_runtime_send_failure(&failure);
        }
        RuntimeEvent::Lagged { count } => {
            tracing::warn!("Gemini runtime event stream lagged by {count} events");
        }
        RuntimeEvent::ToolCallStarted { id, name } => {
            tracing::info!("tool call started: {name} ({id})");
        }
        RuntimeEvent::ToolCallFinished { id, name, outcome } => {
            tracing::info!("tool call finished: {name} ({id}) -> {outcome:?}");
        }
        RuntimeEvent::Server(server_event) => {
            handle_server_event(state, reply_state, http, server_event).await?;
        }
    }
    Ok(())
}

async fn handle_server_event(
    state: &SharedBotState,
    reply_state: &mut RuntimeReplyState,
    http: &Arc<Http>,
    event: ServerEvent,
) -> Result<(), DiscordServiceError> {
    match event {
        ServerEvent::ModelAudio(data) => {
            if let Err(error) = state.push_model_audio(data).await {
                tracing::warn!("failed to push model audio into Discord voice bridge: {error}");
                let _ = state.leave_voice().await;
            }
        }
        ServerEvent::OutputTranscription(text) => {
            state
                .record_runtime_activity(ActivityKind::ModelOutput, Instant::now())
                .await;
            tracing::info!("Gemini output transcription: {text}");
            if ensure_active_reply(state, reply_state).await.is_some() {
                reply_state.append_output_transcription(&text);
            }
        }
        ServerEvent::ModelText(text) => {
            state
                .record_runtime_activity(ActivityKind::ModelOutput, Instant::now())
                .await;
            if ensure_active_reply(state, reply_state).await.is_some() {
                reply_state.append_model_text(&text);
            }
        }
        ServerEvent::TurnComplete => {
            let _ = ensure_active_reply(state, reply_state).await;
            if let Some(turn) = reply_state.finish_turn() {
                if !turn.rendered_text.is_empty() {
                    send_discord_text(http, turn.channel_id, &turn.rendered_text).await?;
                }
                state.record_completed_turn(&turn).await;
            }
        }
        ServerEvent::Interrupted => {
            state.clear_model_audio().await;
            reply_state.discard_active();
            tracing::debug!("Gemini Live turn was interrupted");
        }
        ServerEvent::InputTranscription(text) => {
            state
                .record_runtime_activity(ActivityKind::VoiceInput, Instant::now())
                .await;
            tracing::info!("Gemini input transcription: {text}");
            if ensure_active_reply(state, reply_state).await.is_some() {
                reply_state.append_input_transcription(&text);
            }
        }
        ServerEvent::GenerationComplete => {}
        ServerEvent::SetupComplete => {}
        ServerEvent::ToolCall(_) => {}
        ServerEvent::ToolCallCancellation(_) => {}
        ServerEvent::SessionResumption { .. } => {
            state.sync_resume_handle_from_runtime(Instant::now()).await;
        }
        ServerEvent::GoAway { time_left } => {
            tracing::warn!("Gemini Live requested reconnect; time left: {time_left:?}");
        }
        ServerEvent::Usage(usage) => {
            tracing::debug!(
                "Gemini usage update: total_tokens={}",
                usage.total_token_count
            );
        }
        ServerEvent::Closed { reason } => {
            tracing::warn!("Gemini Live session closed: {reason}");
        }
        ServerEvent::Error(error) => {
            tracing::warn!("Gemini Live API error: {}", error.message);
        }
    }
    Ok(())
}

async fn ensure_active_reply(
    state: &SharedBotState,
    reply_state: &mut RuntimeReplyState,
) -> Option<PendingTextReply> {
    if reply_state.active.is_some() {
        return reply_state.active.as_ref().map(|active| PendingTextReply {
            channel_id: active.channel_id,
            prompt: active.user_input.clone(),
        });
    }
    if let Some(pending) = state.next_pending_text_reply().await {
        reply_state.active = Some(ChatReplyAccumulator::for_text_turn(
            pending.channel_id,
            pending.prompt.clone(),
        ));
        return Some(pending);
    }
    let channel_id = state.target_channel_id().await?;
    reply_state.active = Some(ChatReplyAccumulator::for_voice_turn(channel_id));
    Some(PendingTextReply {
        channel_id,
        prompt: String::new(),
    })
}

fn spawn_idle_dormancy_loop(state: SharedBotState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(IDLE_CHECK_INTERVAL);
        loop {
            interval.tick().await;
            if let Err(error) = state.maybe_enter_dormant_if_idle(Instant::now()).await {
                tracing::warn!("idle dormancy evaluation failed: {error}");
            }
        }
    })
}

fn log_runtime_lifecycle(event: &RuntimeLifecycleEvent) {
    match event {
        RuntimeLifecycleEvent::Connecting => tracing::info!("connecting Gemini Live session"),
        RuntimeLifecycleEvent::Connected => tracing::info!("Gemini Live session connected"),
        RuntimeLifecycleEvent::Reconnecting => tracing::warn!("Gemini Live session reconnecting"),
        RuntimeLifecycleEvent::AppliedResumedSession => {
            tracing::info!("Gemini Live resumed onto a new session")
        }
        RuntimeLifecycleEvent::AppliedFreshSession => {
            tracing::warn!("Gemini Live switched to a fresh session")
        }
        RuntimeLifecycleEvent::Closed { reason } if reason.is_empty() => {
            tracing::info!("Gemini Live session closed")
        }
        RuntimeLifecycleEvent::Closed { reason } => {
            tracing::warn!("Gemini Live session closed: {reason}")
        }
    }
}

fn log_runtime_send_failure(failure: &RuntimeSendFailure) {
    tracing::warn!(
        "Gemini outbound {:?} failed: {}",
        failure.operation,
        failure.reason
    );
}

fn discord_songbird_config() -> songbird::Config {
    let mut config = songbird::Config::default();
    config.decode_mode = DecodeMode::Decode(DecodeConfig::new(Channels::Mono, SampleRate::Hz16000));
    config
}

async fn print_invite_link(http: &Arc<Http>, guild_id: GuildId) {
    match fetch_invite_url(http, guild_id).await {
        Ok(url) => println!("Discord invite link: {url}"),
        Err(error) => tracing::warn!("failed to resolve Discord invite link: {error}"),
    }
}

async fn fetch_invite_url(
    http: &Arc<Http>,
    guild_id: GuildId,
) -> Result<String, DiscordServiceError> {
    let application_info = http.get_current_application_info().await?;
    Ok(build_invite_url(application_info.id, guild_id))
}

fn build_invite_url(application_id: ApplicationId, guild_id: GuildId) -> String {
    CreateBotAuthParameters::new()
        .client_id(application_id)
        .scopes(&[Scope::Bot])
        .permissions(required_bot_permissions())
        .guild_id(guild_id)
        .disable_guild_select(true)
        .build()
}

fn required_bot_permissions() -> Permissions {
    Permissions::VIEW_CHANNEL
        | Permissions::SEND_MESSAGES
        | Permissions::READ_MESSAGE_HISTORY
        | Permissions::CONNECT
        | Permissions::SPEAK
        | Permissions::USE_VAD
        | Permissions::MANAGE_CHANNELS
}

fn format_user_message_for_model(message: &Message) -> String {
    format_user_message_for_model_parts(&message.author.name, &message.content)
}

fn format_user_message_for_model_parts(author_name: &str, content: &str) -> String {
    format!("Discord user {author_name} says: {content}")
}

fn content_from_turn_text(role: &str, text: String) -> Option<Content> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(Content {
        role: Some(role.to_owned()),
        parts: vec![Part {
            text: Some(trimmed.to_owned()),
            inline_data: None,
        }],
    })
}

async fn send_discord_text(
    http: &Arc<Http>,
    channel_id: ChannelId,
    text: &str,
) -> Result<(), DiscordServiceError> {
    for chunk in split_for_discord(text) {
        channel_id.say(http, chunk).await?;
    }
    Ok(())
}

fn split_for_discord(text: &str) -> Vec<String> {
    const LIMIT: usize = 2_000;
    if text.len() <= LIMIT {
        return vec![text.to_owned()];
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if current.len() >= LIMIT {
            chunks.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use serenity::all::{GuildId, UserId};

    use super::*;

    fn config() -> DiscordBotConfig {
        DiscordBotConfig {
            discord_bot_token: "discord-token".into(),
            gemini_api_key: "gemini-key".into(),
            guild_id: GuildId::new(123),
            owner_user_id: UserId::new(456),
            voice_channel_name: "gemini-live".into(),
            model: "models/custom-live".into(),
            idle_timeout: Duration::from_secs(90),
            max_recent_turns: 24,
        }
    }

    #[test]
    fn prepare_keeps_original_config() {
        let service = DiscordAgentService::new(config());
        let prepared = service.prepare();

        assert_eq!(prepared.config().voice_channel_name, "gemini-live");
        assert_eq!(prepared.state(), &DiscordServiceState::default());
    }

    #[test]
    fn formats_discord_prompt_with_author_name() {
        assert_eq!(
            format_user_message_for_model_parts("alice", "hello"),
            "Discord user alice says: hello"
        );
    }

    #[test]
    fn splits_long_messages_for_discord() {
        let chunks = split_for_discord(&"a".repeat(4_500));
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 2_000);
        assert_eq!(chunks[1].len(), 2_000);
        assert_eq!(chunks[2].len(), 500);
    }

    #[test]
    fn text_turns_still_render_text_projection() {
        let mut reply_state = RuntimeReplyState::new();
        reply_state.active = Some(ChatReplyAccumulator::for_text_turn(
            ChannelId::new(10),
            "Discord user alice says: hello".into(),
        ));
        reply_state.append_output_transcription("spoken text turn");

        let completed = reply_state.finish_turn().expect("completed turn");
        assert_eq!(completed.channel_id, ChannelId::new(10));
        assert_eq!(completed.rendered_text, "spoken text turn");
        assert_eq!(
            completed.user_turn,
            content_from_turn_text("user", "Discord user alice says: hello".into())
        );
        assert_eq!(
            completed.model_turn,
            content_from_turn_text("model", "spoken text turn".into())
        );
    }

    #[test]
    fn text_turn_reply_falls_back_to_model_text() {
        let mut reply_state = RuntimeReplyState::new();
        reply_state.active = Some(ChatReplyAccumulator::for_text_turn(
            ChannelId::new(10),
            "Discord user alice says: hello".into(),
        ));
        reply_state.append_model_text("hello from text");

        let completed = reply_state.finish_turn().expect("completed turn");
        assert_eq!(completed.channel_id, ChannelId::new(10));
        assert_eq!(completed.rendered_text, "hello from text");
        assert_eq!(
            completed.model_turn,
            content_from_turn_text("model", "hello from text".into())
        );
    }

    #[test]
    fn voice_turn_reply_only_projects_output_transcription() {
        let mut reply_state = RuntimeReplyState::new();
        reply_state.active = Some(ChatReplyAccumulator::for_voice_turn(ChannelId::new(10)));
        reply_state.append_input_transcription("hello from voice");
        reply_state.append_model_text("internal-only");

        reply_state.append_output_transcription("spoken reply");
        let completed = reply_state.finish_turn().expect("completed turn");
        assert_eq!(completed.channel_id, ChannelId::new(10));
        assert_eq!(completed.rendered_text, "spoken reply");
        assert_eq!(
            completed.user_turn,
            content_from_turn_text("user", "hello from voice".into())
        );
        assert_eq!(
            completed.model_turn,
            content_from_turn_text("model", "spoken reply".into())
        );
    }

    #[test]
    fn invite_url_is_prefilled_for_target_guild() {
        let url = build_invite_url(ApplicationId::new(321), GuildId::new(123));

        assert!(url.contains("client_id=321"));
        assert!(url.contains("guild=123"));
        assert!(url.contains("disable_guild_select=true"));
        assert!(url.contains("scope=bot"));
        assert!(url.contains(&format!(
            "permissions={}",
            required_bot_permissions().bits()
        )));
    }

    #[test]
    fn required_permissions_cover_channel_setup_and_voice() {
        let permissions = required_bot_permissions();

        assert!(permissions.contains(Permissions::MANAGE_CHANNELS));
        assert!(permissions.contains(Permissions::VIEW_CHANNEL));
        assert!(permissions.contains(Permissions::SEND_MESSAGES));
        assert!(permissions.contains(Permissions::READ_MESSAGE_HISTORY));
        assert!(permissions.contains(Permissions::CONNECT));
        assert!(permissions.contains(Permissions::SPEAK));
        assert!(permissions.contains(Permissions::USE_VAD));
    }
}
