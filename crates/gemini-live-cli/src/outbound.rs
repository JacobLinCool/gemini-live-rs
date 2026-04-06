//! User-input/media sending helpers for the CLI host.
//!
//! This module keeps `@file` expansion and outbound send ordering testable
//! without requiring a running Live session.

use std::time::Duration;

use gemini_live::SessionError;
use gemini_live_runtime::RuntimeSession;

use crate::app::App;
use crate::media;

const MEDIA_BEFORE_TEXT_DELAY: Duration = Duration::from_millis(500);

pub(crate) async fn send_user_input<S>(
    app: &mut App,
    session: &S,
    line: &str,
) -> Result<(), SessionError>
where
    S: RuntimeSession,
{
    send_user_input_with_delay(app, session, line, MEDIA_BEFORE_TEXT_DELAY).await
}

async fn send_user_input_with_delay<S>(
    app: &mut App,
    session: &S,
    line: &str,
    delay_after_media: Duration,
) -> Result<(), SessionError>
where
    S: RuntimeSession,
{
    let (text, file_paths) = media::parse_input(line);

    app.push_user_message(line.to_string());

    for path in &file_paths {
        match media::load(path) {
            Ok(m) => {
                app.sys(media::describe(path, &m));
                match m {
                    media::Media::Image { data, mime } => {
                        session.send_video(&data, mime).await?;
                    }
                    media::Media::Audio { pcm, sample_rate } => {
                        session.send_audio_at_rate(&pcm, sample_rate).await?;
                    }
                }
            }
            Err(e) => app.sys(format!("[skip] @{path}: {e}")),
        }
    }

    if !text.is_empty() {
        if !file_paths.is_empty() {
            tokio::time::sleep(delay_after_media).await;
        }
        session.send_text(&text).await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::app::{App, Role};
    use crate::tooling::ToolProfile;

    #[derive(Clone, Default)]
    struct FakeSession {
        sends: Arc<Mutex<Vec<String>>>,
    }

    impl RuntimeSession for FakeSession {
        fn status(&self) -> gemini_live::SessionStatus {
            gemini_live::SessionStatus::Connected
        }

        fn send_raw<'a>(
            &'a self,
            _message: gemini_live::types::ClientMessage,
        ) -> futures_util::future::BoxFuture<'a, Result<(), SessionError>> {
            Box::pin(async { Ok(()) })
        }

        fn next_event<'a>(
            &'a mut self,
        ) -> futures_util::future::BoxFuture<'a, Option<gemini_live::types::ServerEvent>> {
            Box::pin(async { None })
        }

        fn close(self) -> futures_util::future::BoxFuture<'static, Result<(), SessionError>>
        where
            Self: Sized,
        {
            Box::pin(async { Ok(()) })
        }

        fn send_text<'a>(
            &'a self,
            text: &'a str,
        ) -> futures_util::future::BoxFuture<'a, Result<(), SessionError>> {
            let sends = Arc::clone(&self.sends);
            let text = text.to_string();
            Box::pin(async move {
                sends
                    .lock()
                    .expect("sends lock")
                    .push(format!("text:{text}"));
                Ok(())
            })
        }

        fn send_audio_at_rate<'a>(
            &'a self,
            pcm: &'a [u8],
            sample_rate: u32,
        ) -> futures_util::future::BoxFuture<'a, Result<(), SessionError>> {
            let sends = Arc::clone(&self.sends);
            let len = pcm.len();
            Box::pin(async move {
                sends
                    .lock()
                    .expect("sends lock")
                    .push(format!("audio:{sample_rate}:{len}"));
                Ok(())
            })
        }

        fn send_video<'a>(
            &'a self,
            data: &'a [u8],
            mime: &'a str,
        ) -> futures_util::future::BoxFuture<'a, Result<(), SessionError>> {
            let sends = Arc::clone(&self.sends);
            let mime = mime.to_string();
            let len = data.len();
            Box::pin(async move {
                sends
                    .lock()
                    .expect("sends lock")
                    .push(format!("video:{mime}:{len}"));
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn text_only_input_sends_text_and_records_user_message() {
        let mut app = App::new("test", ToolProfile::default(), None);
        let session = FakeSession::default();

        send_user_input_with_delay(&mut app, &session, "hello", Duration::ZERO)
            .await
            .expect("send input");

        assert!(
            app.messages
                .iter()
                .any(|msg| matches!(msg.role, Role::User) && msg.text == "hello")
        );
        assert_eq!(
            session.sends.lock().expect("sends lock").as_slice(),
            ["text:hello"]
        );
    }

    #[tokio::test]
    async fn media_and_text_input_sends_audio_before_text() {
        let temp_file = temp_raw_pcm_file();
        let input = format!("@{} hello", temp_file.display());
        let mut app = App::new("test", ToolProfile::default(), None);
        let session = FakeSession::default();

        send_user_input_with_delay(&mut app, &session, &input, Duration::ZERO)
            .await
            .expect("send input");

        let sends = session.sends.lock().expect("sends lock").clone();
        assert_eq!(
            sends,
            vec!["audio:16000:4".to_string(), "text:hello".to_string()]
        );
        assert!(app.messages.iter().any(|msg| msg.text.contains("[audio]")));

        let _ = std::fs::remove_file(temp_file);
    }

    #[tokio::test]
    async fn missing_media_file_is_reported_and_text_still_sends() {
        let mut app = App::new("test", ToolProfile::default(), None);
        let session = FakeSession::default();

        send_user_input_with_delay(&mut app, &session, "@missing.raw hello", Duration::ZERO)
            .await
            .expect("send input");

        assert!(
            app.messages
                .iter()
                .any(|msg| msg.text.contains("[skip] @missing.raw"))
        );
        assert_eq!(
            session.sends.lock().expect("sends lock").as_slice(),
            ["text:hello"]
        );
    }

    fn temp_raw_pcm_file() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("gemini-live-cli-test-{unique}.raw"));
        std::fs::write(&path, [0_u8, 1, 2, 3]).expect("write temp raw pcm");
        path
    }
}
