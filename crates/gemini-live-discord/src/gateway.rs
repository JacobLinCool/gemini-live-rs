//! Discord gateway requirements for the bot host.
//!
//! The configured product behavior needs:
//!
//! - `GUILDS` to discover the target guild and observe channel metadata
//! - `GUILD_MESSAGES` to receive text chat in the target voice channel
//! - `MESSAGE_CONTENT` because the bot must read message text
//! - `GUILD_VOICE_STATES` because owner-triggered join/leave and Songbird both
//!   depend on voice-state events

use serenity::all::GatewayIntents;

pub fn gateway_intents() -> GatewayIntents {
    GatewayIntents::GUILDS
        | GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILD_VOICE_STATES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn includes_required_intents() {
        let intents = gateway_intents();

        assert!(intents.contains(GatewayIntents::GUILDS));
        assert!(intents.contains(GatewayIntents::GUILD_MESSAGES));
        assert!(intents.contains(GatewayIntents::MESSAGE_CONTENT));
        assert!(intents.contains(GatewayIntents::GUILD_VOICE_STATES));
    }
}
