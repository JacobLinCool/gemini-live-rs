//! Pure routing policy for the single-guild Discord bot.
//!
//! These helpers deliberately stay independent of the actual Serenity event
//! loop so the product rules remain cheap to test:
//!
//! - text is accepted only in the configured target channel
//! - bot-authored messages are ignored
//! - voice capture is accepted only from the configured owner
//! - join/leave decisions are driven by the owner's movement in the target
//!   voice channel

use serenity::all::{ChannelId, UserId};

/// Fixed conversation scope for a running bot instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotConversationScope {
    pub channel_id: ChannelId,
    pub owner_user_id: UserId,
}

impl BotConversationScope {
    pub fn accepts_text_message(&self, channel_id: ChannelId, is_bot_author: bool) -> bool {
        channel_id == self.channel_id && !is_bot_author
    }

    pub fn accepts_voice_speaker(&self, user_id: UserId) -> bool {
        user_id == self.owner_user_id
    }

    pub fn owner_joined_target_channel(
        &self,
        user_id: UserId,
        previous_channel: Option<ChannelId>,
        new_channel: Option<ChannelId>,
    ) -> bool {
        user_id == self.owner_user_id
            && previous_channel != Some(self.channel_id)
            && new_channel == Some(self.channel_id)
    }

    pub fn owner_left_target_channel(
        &self,
        user_id: UserId,
        previous_channel: Option<ChannelId>,
        new_channel: Option<ChannelId>,
    ) -> bool {
        user_id == self.owner_user_id
            && previous_channel == Some(self.channel_id)
            && new_channel != Some(self.channel_id)
    }
}

#[cfg(test)]
mod tests {
    use serenity::all::{ChannelId, UserId};

    use super::*;

    #[test]
    fn text_is_restricted_to_target_channel_and_non_bot_authors() {
        let scope = BotConversationScope {
            channel_id: ChannelId::new(10),
            owner_user_id: UserId::new(20),
        };

        assert!(scope.accepts_text_message(ChannelId::new(10), false));
        assert!(!scope.accepts_text_message(ChannelId::new(11), false));
        assert!(!scope.accepts_text_message(ChannelId::new(10), true));
    }

    #[test]
    fn voice_is_restricted_to_owner() {
        let scope = BotConversationScope {
            channel_id: ChannelId::new(10),
            owner_user_id: UserId::new(20),
        };

        assert!(scope.accepts_voice_speaker(UserId::new(20)));
        assert!(!scope.accepts_voice_speaker(UserId::new(21)));
    }

    #[test]
    fn owner_join_leave_rules_follow_target_channel() {
        let scope = BotConversationScope {
            channel_id: ChannelId::new(10),
            owner_user_id: UserId::new(20),
        };

        assert!(
            scope.owner_joined_target_channel(UserId::new(20), None, Some(ChannelId::new(10)),)
        );
        assert!(!scope.owner_joined_target_channel(
            UserId::new(21),
            None,
            Some(ChannelId::new(10)),
        ));
        assert!(scope.owner_left_target_channel(UserId::new(20), Some(ChannelId::new(10)), None,));
        assert!(!scope.owner_left_target_channel(UserId::new(20), Some(ChannelId::new(11)), None,));
    }
}
