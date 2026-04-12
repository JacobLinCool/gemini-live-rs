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
//! - deliver queued harness notifications as normal text turns, waking the
//!   shared Live session on demand when needed

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use gemini_live::ServerEvent;
use gemini_live::types::{Content, FunctionResponse, Part};
use gemini_live_harness::{Harness, HarnessToolCompletionDisposition, PassiveNotificationDelivery};
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
use tokio::sync::{Mutex, Notify, RwLock};
use tokio::task::JoinHandle;

use crate::config::{DiscordBotConfig, harness_profile_name};
use crate::error::DiscordServiceError;
use crate::gateway::gateway_intents;
use crate::policy::BotConversationScope;
use crate::session::{
    DiscordHarnessController, DiscordHarnessRuntimeBridge, DiscordSessionManager,
    new_session_manager_with_harness,
};
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

    pub fn prepare(&self) -> Result<PreparedDiscordService, DiscordServiceError> {
        self.prepare_with_harness(Harness::open_profile(&harness_profile_name(
            self.config.guild_id,
        ))?)
    }

    pub(crate) fn prepare_with_harness(
        &self,
        harness: Harness,
    ) -> Result<PreparedDiscordService, DiscordServiceError> {
        let (session_manager, harness_controller, runtime_events) =
            new_session_manager_with_harness(&self.config, harness.clone())?;
        Ok(PreparedDiscordService {
            config: self.config.clone(),
            session_manager,
            harness_controller,
            runtime_events,
            state: DiscordServiceState::default(),
        })
    }
}

/// Prepared service instance with a configured Gemini Live runtime.
pub struct PreparedDiscordService {
    config: DiscordBotConfig,
    session_manager: DiscordSessionManager,
    harness_controller: DiscordHarnessController,
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
    harness_controller: DiscordHarnessController,
    service_state: RwLock<DiscordServiceState>,
    pending_text_replies: Mutex<VecDeque<PendingTextReply>>,
    turn_in_flight: Mutex<bool>,
    voice_bridge: Mutex<Option<ActiveVoiceBridge>>,
    idle_state_changed: Notify,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingTextReply {
    channel_id: ChannelId,
    prompt: String,
    notification_id: Option<String>,
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
    notification_id: Option<String>,
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
        let shared_state = SharedBotState::new(
            self.config.clone(),
            self.session_manager,
            self.harness_controller,
        );
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
        let recovered_count = shared_state.recover_orphaned_notification_deliveries()?;
        if recovered_count > 0 {
            tracing::info!("requeued {recovered_count} orphaned harness notifications");
        }

        let runtime_task =
            spawn_runtime_event_loop(shared_state.clone(), self.runtime_events, http);
        let idle_task = spawn_idle_dormancy_loop(shared_state.clone());
        let notification_task = spawn_notification_delivery_loop(shared_state.clone());
        let start_result = client.start().await;
        runtime_task.abort();
        idle_task.abort();
        notification_task.abort();
        let close_result = shared_state.close_runtime().await;

        start_result?;
        close_result?;
        Ok(())
    }
}

impl SharedBotState {
    fn new(
        config: DiscordBotConfig,
        session_manager: DiscordSessionManager,
        harness_controller: DiscordHarnessController,
    ) -> Self {
        Self {
            inner: Arc::new(SharedBotStateInner {
                config,
                session_manager: Mutex::new(session_manager),
                harness_controller,
                service_state: RwLock::new(DiscordServiceState::default()),
                pending_text_replies: Mutex::new(VecDeque::new()),
                turn_in_flight: Mutex::new(false),
                voice_bridge: Mutex::new(None),
                idle_state_changed: Notify::new(),
            }),
        }
    }

    fn note_idle_state_changed(&self) {
        self.inner.idle_state_changed.notify_waiters();
    }

    fn note_passive_notification_gate_changed(&self) {
        self.inner
            .harness_controller
            .notify_passive_notification_gate_changed();
    }

    async fn wait_for_idle_state_change(&self) {
        self.inner.idle_state_changed.notified().await;
    }

    fn config(&self) -> &DiscordBotConfig {
        &self.inner.config
    }

    fn harness_controller(&self) -> &DiscordHarnessController {
        &self.inner.harness_controller
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
                drop(state);
                self.note_passive_notification_gate_changed();
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
                notification_id: None,
            });
        self.set_turn_in_flight(true).await;
        let mut session_manager = self.inner.session_manager.lock().await;
        session_manager
            .ensure_hot(WakeReason::TextInput, Instant::now())
            .await?;
        if let Err(error) = session_manager.runtime().send_text(&prompt).await {
            let _ = self.inner.pending_text_replies.lock().await.pop_back();
            self.sync_turn_state_after_reply_end().await;
            return Err(DiscordServiceError::from(error));
        }
        Ok(())
    }

    async fn next_pending_text_reply(&self) -> Option<PendingTextReply> {
        self.inner.pending_text_replies.lock().await.pop_front()
    }

    async fn is_turn_in_flight(&self) -> bool {
        *self.inner.turn_in_flight.lock().await
    }

    async fn set_turn_in_flight(&self, value: bool) {
        *self.inner.turn_in_flight.lock().await = value;
        self.note_idle_state_changed();
        self.note_passive_notification_gate_changed();
    }

    async fn sync_turn_state_after_reply_end(&self) {
        let pending = !self.inner.pending_text_replies.lock().await.is_empty();
        self.set_turn_in_flight(pending).await;
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
        drop(state);
        self.note_idle_state_changed();
        Ok(())
    }

    async fn leave_voice(&self) -> Result<(), DiscordServiceError> {
        let existing = self.inner.voice_bridge.lock().await.take();
        if let Some(bridge) = existing {
            bridge.shutdown().await?;
        }

        let mut state = self.inner.service_state.write().await;
        state.joined_voice_channel_id = None;
        drop(state);
        self.note_idle_state_changed();
        Ok(())
    }

    async fn push_model_audio(&self, pcm_i16_le_24k: Bytes) -> Result<(), DiscordServiceError> {
        let guard = self.inner.voice_bridge.lock().await;
        if let Some(bridge) = guard.as_ref() {
            bridge.push_model_audio(pcm_i16_le_24k).await?;
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
        self.requeue_in_flight_notification()?;
        let mut session_manager = self.inner.session_manager.lock().await;
        session_manager.enter_dormant().await?;
        self.set_turn_in_flight(false).await;
        Ok(())
    }

    async fn send_tool_response(
        &self,
        responses: Vec<FunctionResponse>,
    ) -> Result<(), DiscordServiceError> {
        let session_manager = self.inner.session_manager.lock().await;
        session_manager
            .runtime()
            .send_tool_response(responses)
            .await?;
        Ok(())
    }

    async fn sync_resume_handle_from_runtime(&self, issued_at: Instant) {
        let session_manager = self.inner.session_manager.lock().await;
        session_manager.sync_resume_handle_from_runtime(issued_at);
    }

    async fn record_runtime_activity(&self, kind: ActivityKind, at: Instant) {
        let session_manager = self.inner.session_manager.lock().await;
        session_manager.record_activity(kind, at);
        drop(session_manager);
        self.note_idle_state_changed();
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
        if self.is_turn_in_flight().await {
            return Ok(());
        }

        let mut session_manager = self.inner.session_manager.lock().await;
        if session_manager.lifecycle_state() == SessionLifecycleState::Dormant {
            return Ok(());
        }
        if session_manager.idle_decision(now) == gemini_live_runtime::IdleDecision::EnterDormant {
            session_manager.enter_dormant().await?;
            drop(session_manager);
            self.note_idle_state_changed();
        }
        Ok(())
    }

    async fn next_idle_deadline(&self) -> Option<Instant> {
        if self.inner.voice_bridge.lock().await.is_some() {
            return None;
        }
        if self.is_turn_in_flight().await {
            return None;
        }

        let session_manager = self.inner.session_manager.lock().await;
        if session_manager.lifecycle_state() == SessionLifecycleState::Dormant {
            return None;
        }

        let snapshot = session_manager.snapshot();
        Some(
            snapshot
                .last_activity_at
                .map(|at| at + session_manager.idle_policy().idle_timeout)
                .unwrap_or_else(Instant::now),
        )
    }

    fn recover_orphaned_notification_deliveries(&self) -> Result<usize, DiscordServiceError> {
        Ok(self
            .inner
            .harness_controller
            .recover_orphaned_deliveries()?)
    }

    async fn can_deliver_passive_notification(&self) -> bool {
        self.target_channel_id().await.is_some() && !self.is_turn_in_flight().await
    }

    async fn deliver_passive_notification(
        &self,
        delivery: PassiveNotificationDelivery,
    ) -> Result<(), DiscordServiceError> {
        let Some(channel_id) = self.target_channel_id().await else {
            return Ok(());
        };

        self.inner
            .pending_text_replies
            .lock()
            .await
            .push_back(PendingTextReply {
                channel_id,
                prompt: delivery.prompt.clone(),
                notification_id: Some(delivery.notification.id),
            });
        self.set_turn_in_flight(true).await;

        let send_result = {
            let mut session_manager = self.inner.session_manager.lock().await;
            session_manager
                .ensure_hot(WakeReason::PassiveNotification, Instant::now())
                .await?;
            session_manager.runtime().send_text(&delivery.prompt).await
        };
        match send_result {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = self.inner.pending_text_replies.lock().await.pop_back();
                self.sync_turn_state_after_reply_end().await;
                Err(error.into())
            }
        }
    }

    fn acknowledge_in_flight_notification(&self) -> Result<(), DiscordServiceError> {
        let _ = self
            .inner
            .harness_controller
            .acknowledge_in_flight_notification()?;
        Ok(())
    }

    fn requeue_in_flight_notification(&self) -> Result<(), DiscordServiceError> {
        let _ = self
            .inner
            .harness_controller
            .requeue_in_flight_notification()?;
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
    fn for_text_turn(
        channel_id: ChannelId,
        prompt: String,
        notification_id: Option<String>,
    ) -> Self {
        Self {
            origin: ReplyOrigin::TextTurn,
            channel_id,
            user_input: prompt,
            notification_id,
            output_transcription: String::new(),
            model_text: String::new(),
        }
    }

    fn for_voice_turn(channel_id: ChannelId) -> Self {
        Self {
            origin: ReplyOrigin::VoiceTurn,
            channel_id,
            user_input: String::new(),
            notification_id: None,
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
        let mut runtime_bridge =
            DiscordHarnessRuntimeBridge::new(state.harness_controller().clone());

        loop {
            tokio::select! {
                Some(event) = runtime_events.recv() => {
                    if let Err(error) = handle_runtime_event(
                        &state,
                        &mut reply_state,
                        &http,
                        event,
                        &runtime_bridge,
                    ).await {
                        tracing::warn!("runtime projection error: {error}");
                    }
                }
                Some(forwarded) = runtime_bridge.recv_and_forward_tool_completion(|responses| state.send_tool_response(responses)) => {
                    if let Err(error) = handle_tool_execution_forwarding(forwarded).await {
                        tracing::warn!("tool completion handling failed: {error}");
                    }
                }
                else => break,
            }
        }

        state.harness_controller().abort_all_tool_calls();
    })
}

fn spawn_notification_delivery_loop(state: SharedBotState) -> JoinHandle<()> {
    let harness_controller = state.inner.harness_controller.clone();
    harness_controller.spawn_passive_notification_loop(
        {
            let state = state.clone();
            move || {
                let state = state.clone();
                async move { state.can_deliver_passive_notification().await }
            }
        },
        move |delivery| {
            let state = state.clone();
            async move { state.deliver_passive_notification(delivery).await }
        },
    )
}

async fn handle_runtime_event(
    state: &SharedBotState,
    reply_state: &mut RuntimeReplyState,
    http: &Arc<Http>,
    event: RuntimeEvent,
    runtime_bridge: &DiscordHarnessRuntimeBridge,
) -> Result<(), DiscordServiceError> {
    let _ = runtime_bridge.handle_runtime_event(&event);
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
        RuntimeEvent::ToolCallRequested { call } => {
            tracing::info!("tool call requested: {} ({})", call.name, call.id);
        }
        RuntimeEvent::ToolCallCancellationRequested { ids } => {
            for call_id in ids {
                tracing::info!("tool call cancelled: {call_id}");
            }
        }
        RuntimeEvent::Server(server_event) => {
            handle_server_event(state, reply_state, http, server_event).await?;
        }
    }
    Ok(())
}

async fn handle_tool_execution_forwarding(
    forwarded: Result<
        gemini_live_harness::HarnessToolForwardOutcome,
        gemini_live_harness::HarnessToolForwardFailure<DiscordServiceError>,
    >,
) -> Result<(), DiscordServiceError> {
    match forwarded {
        Ok(outcome) => match outcome.disposition {
            HarnessToolCompletionDisposition::Responded => {
                tracing::info!(
                    "tool call responded: {} ({})",
                    outcome.call_name,
                    outcome.call_id
                );
            }
            HarnessToolCompletionDisposition::Failed => {
                tracing::warn!(
                    "tool call failed: {} ({})",
                    outcome.call_name,
                    outcome.call_id
                );
            }
            HarnessToolCompletionDisposition::Cancelled => {
                tracing::info!("tool call cancelled before response: {}", outcome.call_id);
            }
        },
        Err(error) => {
            tracing::warn!(
                "tool completion forwarding failed for {} ({}): {}",
                error.call_name,
                error.call_id,
                error.source
            );
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
            state.acknowledge_in_flight_notification()?;
            state.sync_turn_state_after_reply_end().await;
        }
        ServerEvent::Interrupted => {
            state.clear_model_audio().await;
            reply_state.discard_active();
            state.requeue_in_flight_notification()?;
            state.sync_turn_state_after_reply_end().await;
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
            state.requeue_in_flight_notification()?;
            state.set_turn_in_flight(false).await;
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
            notification_id: active.notification_id.clone(),
        });
    }
    if let Some(pending) = state.next_pending_text_reply().await {
        reply_state.active = Some(ChatReplyAccumulator::for_text_turn(
            pending.channel_id,
            pending.prompt.clone(),
            pending.notification_id.clone(),
        ));
        return Some(pending);
    }
    let channel_id = state.target_channel_id().await?;
    reply_state.active = Some(ChatReplyAccumulator::for_voice_turn(channel_id));
    Some(PendingTextReply {
        channel_id,
        prompt: String::new(),
        notification_id: None,
    })
}

fn spawn_idle_dormancy_loop(state: SharedBotState) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Some(deadline) = state.next_idle_deadline().await {
                let sleep = tokio::time::sleep(deadline.saturating_duration_since(Instant::now()));
                tokio::pin!(sleep);
                tokio::select! {
                    _ = &mut sleep => {
                        if let Err(error) = state.maybe_enter_dormant_if_idle(Instant::now()).await {
                            tracing::warn!("idle dormancy evaluation failed: {error}");
                        }
                    }
                    _ = state.wait_for_idle_state_change() => {}
                }
            } else {
                state.wait_for_idle_state_change().await;
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
    use std::time::{SystemTime, UNIX_EPOCH};

    use gemini_live_harness::{
        Harness, HarnessNotification, NotificationKind, NotificationStatus,
        format_passive_notification_prompt,
    };
    use serenity::all::{GuildId, UserId};

    use super::*;
    use crate::config::DEFAULT_DISCORD_SYSTEM_INSTRUCTION;

    fn config() -> DiscordBotConfig {
        DiscordBotConfig {
            discord_bot_token: "discord-token".into(),
            gemini_api_key: "gemini-key".into(),
            guild_id: GuildId::new(123),
            owner_user_id: UserId::new(456),
            voice_channel_name: "gemini-live".into(),
            model: "models/custom-live".into(),
            thinking_level: gemini_live::types::ThinkingLevel::High,
            system_instruction: DEFAULT_DISCORD_SYSTEM_INSTRUCTION.into(),
            voice_name: None,
            idle_timeout: std::time::Duration::from_secs(90),
            max_recent_turns: 24,
        }
    }

    fn temp_harness() -> Harness {
        let path = std::env::temp_dir().join(format!(
            "gemini-live-discord-service-harness-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time before unix epoch")
                .as_nanos()
        ));
        Harness::open(path).expect("open test harness")
    }

    #[test]
    fn prepare_keeps_original_config() {
        let service = DiscordAgentService::new(config());
        let prepared = service
            .prepare_with_harness(temp_harness())
            .expect("prepare service");

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
    fn passive_notification_prompt_comes_from_harness() {
        let prompt = format_passive_notification_prompt(&HarnessNotification {
            id: "notification_1".into(),
            kind: NotificationKind::TaskSucceeded,
            created_at_ms: 1,
            updated_at_ms: 1,
            status: NotificationStatus::Queued,
            task_id: Some("task_123".into()),
            title: "Task completed".into(),
            body: "The background work finished.".into(),
            payload: None,
            delivered_at_ms: None,
            acknowledged_at_ms: None,
        });
        assert!(prompt.contains("Notification ID:"));
        assert!(prompt.contains("Task ID: task_123"));
        assert!(prompt.contains("Title: Task completed"));
        assert!(prompt.contains("Body: The background work finished."));
    }

    #[tokio::test]
    async fn passive_notification_gate_does_not_require_an_active_session() {
        let prepared = DiscordAgentService::new(config())
            .prepare_with_harness(temp_harness())
            .expect("prepare service");
        let state = SharedBotState::new(
            prepared.config.clone(),
            prepared.session_manager,
            prepared.harness_controller,
        );

        state.inner.service_state.write().await.target_channel_id = Some(ChannelId::new(10));

        assert!(state.can_deliver_passive_notification().await);
    }

    #[tokio::test]
    async fn passive_notification_gate_still_blocks_while_a_turn_is_in_flight() {
        let prepared = DiscordAgentService::new(config())
            .prepare_with_harness(temp_harness())
            .expect("prepare service");
        let state = SharedBotState::new(
            prepared.config.clone(),
            prepared.session_manager,
            prepared.harness_controller,
        );

        state.inner.service_state.write().await.target_channel_id = Some(ChannelId::new(10));
        state.set_turn_in_flight(true).await;

        assert!(!state.can_deliver_passive_notification().await);
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
            None,
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
            None,
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
