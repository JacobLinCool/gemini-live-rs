# gemini-live-discord

Discord host crate for a single-guild, single-owner Gemini Live agent.

This crate is the home for the Discord bot service that pairs:

- `gemini-live` for the Live API wire client
- `gemini-live-runtime` for long-lived session orchestration
- `serenity` for the Discord gateway and HTTP surface
- `songbird` for Discord voice ingress and egress

## Product Contract

The target product behavior is intentionally narrow:

- Exactly one configured Discord guild
- Exactly one configured owner user
- Exactly one configured voice-channel name
- Text and voice share one Gemini Live session context
- Voice interaction is restricted to the owner
- Text interaction is restricted to the configured voice channel's chat surface

### Startup and Setup

On startup, or when the bot later receives a guild-join event for the target
guild, the service should:

1. Locate a voice channel whose name exactly matches `DISCORD_VOICE_CHANNEL_NAME`.
2. Reuse that channel if it already exists.
3. Otherwise attempt to create a new voice channel with that name.
4. If creation fails because of permissions, surface that state and retry on
   the next restart or guild sync.

Discord voice channels now expose their own text chat surface, so this design
uses one channel id for both voice-presence rules and text-message routing.

### Interaction Rules

Text:

- Any user posting in the configured voice channel's chat should receive a text
  reply there.
- Messages outside that channel should be ignored.
- If the bot is currently connected in voice, text-triggered replies should
  still be spoken there in addition to being projected into the channel chat.

Voice:

- The bot should auto-join only when the configured owner enters the configured
  voice channel.
- Once joined, the bot should only listen and respond to the owner's voice.
- The bot should auto-leave when the owner leaves that voice channel.

Session:

- Text and voice share one Live session context.
- Leaving the voice channel should tear down the Discord voice bridge, not
  necessarily discard the Live session state used by text chat.
- The Live session may go dormant after idle time and later wake by resuming
  the last good handle or by rehydrating from process-local recent turns.

## Environment

Required:

- `DISCORD_BOT_TOKEN`
- `GEMINI_API_KEY`
- `DISCORD_GUILD_ID`
- `DISCORD_OWNER_USER_ID`
- `DISCORD_VOICE_CHANNEL_NAME`

Optional:

- `GEMINI_MODEL`
  - Defaults to `models/gemini-3.1-flash-live-preview`
- `DISCORD_SESSION_IDLE_TIMEOUT_SECS`
  - Defaults to `600`
- `DISCORD_SESSION_MAX_RECENT_TURNS`
  - Defaults to `16`

## Running

```bash
export DISCORD_BOT_TOKEN=...
export GEMINI_API_KEY=...
export DISCORD_GUILD_ID=...
export DISCORD_OWNER_USER_ID=...
export DISCORD_VOICE_CHANNEL_NAME=gemini-live

cargo run -p gemini-live-discord
```

Startup prints an OAuth2 invite link prefilled for `DISCORD_GUILD_ID` with the
permissions this bot needs for channel setup, text chat, and voice.

For text-chat support, Discord Developer Portal must also have `MESSAGE CONTENT
INTENT` enabled for this bot. Without it, startup fails with
`DisallowedGatewayIntents`.

## Current Implementation

The crate now includes:

- environment parsing and validation
- a shared audio-first Gemini Live session bootstrap
- a built-in system instruction that grounds the model in Discord voice/chat
- server-side context compression and initial-history support for fresh wakes
- Discord gateway handling through `serenity`
- target voice-channel discovery or creation through Discord HTTP
- an owner-only Songbird receive/playback bridge
- a lazy hot/dormant session manager with in-memory recent-turn continuity

What is still intentionally narrow:

- one configured guild only
- one configured owner only
- one shared Live session only
- no persistence layer beyond environment variables
- no slash-command surface yet

## Module Map

- `config.rs`
  - environment contract and startup validation
- `session.rs`
  - `gemini-live-runtime` bootstrap for the shared Live session
- `setup.rs`
  - target voice-channel discovery/creation planning
- `policy.rs`
  - owner-only voice rules and target-channel text routing
- `service.rs`
  - top-level Discord service, gateway handlers, and runtime projection
- `gateway.rs`
  - required Discord gateway intents
- `voice.rs`
  - Songbird owner-only receive/playback bridge
