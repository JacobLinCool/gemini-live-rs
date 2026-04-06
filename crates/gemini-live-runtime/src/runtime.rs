//! Reusable staged-session orchestration above a concrete session driver.

use gemini_live::SessionConfig;
use gemini_live::types::{ClientMessage, ServerEvent, SetupConfig};

use crate::config::{RuntimeConfig, SetupPatch};
use crate::driver::{RuntimeSession, SessionDriver};
use crate::error::RuntimeError;

/// Result returned after promoting a staged setup onto a newly connected
/// session.
#[derive(Debug, Clone)]
pub struct ApplyReport {
    pub previous_setup: SetupConfig,
    pub active_setup: SetupConfig,
}

/// Shared runtime that stages `setup` edits and reconnects through a pluggable
/// session driver.
///
/// This runtime intentionally does not own UI, persistence, device I/O, or any
/// concrete tool set. Host applications layer those concerns on top.
pub struct LiveRuntime<D>
where
    D: SessionDriver,
{
    driver: D,
    config: RuntimeConfig,
    active_setup: SetupConfig,
    desired_setup: SetupConfig,
    session: Option<D::Session>,
}

impl<D> LiveRuntime<D>
where
    D: SessionDriver,
{
    pub fn new(config: RuntimeConfig, driver: D) -> Self {
        let active_setup = config.session.setup.clone();
        let desired_setup = active_setup.clone();
        Self {
            driver,
            config,
            active_setup,
            desired_setup,
            session: None,
        }
    }

    pub fn driver(&self) -> &D {
        &self.driver
    }

    pub fn active_setup(&self) -> &SetupConfig {
        &self.active_setup
    }

    pub fn desired_setup(&self) -> &SetupConfig {
        &self.desired_setup
    }

    pub fn stage_patch(&mut self, patch: &SetupPatch) {
        patch.apply_to(&mut self.desired_setup);
    }

    pub fn replace_desired_setup(&mut self, setup: SetupConfig) {
        self.desired_setup = setup;
    }

    pub fn discard_staged_setup(&mut self) {
        self.desired_setup = self.active_setup.clone();
    }

    pub fn session(&self) -> Option<&D::Session> {
        self.session.as_ref()
    }

    pub fn session_mut(&mut self) -> Option<&mut D::Session> {
        self.session.as_mut()
    }

    pub async fn connect(&mut self) -> Result<(), RuntimeError> {
        let next_setup = self.desired_setup.clone();
        let next_session = self
            .driver
            .connect(self.build_session_config(next_setup.clone()))
            .await?;
        self.active_setup = next_setup;
        self.desired_setup = self.active_setup.clone();
        self.session = Some(next_session);
        Ok(())
    }

    /// Connect a session for the current desired setup without installing it.
    ///
    /// Hosts can use this two-phase flow when they need to attach event
    /// forwarders or tear down old task state before the previous session is
    /// closed.
    pub async fn connect_desired_session(&self) -> Result<D::Session, RuntimeError> {
        self.connect_with_setup(self.desired_setup.clone()).await
    }

    /// Connect a session using an explicit setup payload without installing it.
    ///
    /// `ManagedRuntime` uses this to inject a server-issued resumption handle
    /// into a one-off setup clone while keeping `active_setup` /
    /// `desired_setup` handle-free.
    pub(crate) async fn connect_with_setup(
        &self,
        setup: SetupConfig,
    ) -> Result<D::Session, RuntimeError> {
        Ok(self
            .driver
            .connect(self.build_session_config(setup))
            .await?)
    }

    /// Install a session previously connected for the current desired setup.
    ///
    /// Returns the apply report plus the replaced session handle so the host
    /// can close it at an exact point in its own switchover sequence.
    pub fn install_connected_session(
        &mut self,
        next_session: D::Session,
    ) -> (ApplyReport, Option<D::Session>) {
        let previous_setup = self.active_setup.clone();
        let old_session = self.session.replace(next_session);
        self.active_setup = self.desired_setup.clone();
        self.desired_setup = self.active_setup.clone();
        (
            ApplyReport {
                previous_setup,
                active_setup: self.active_setup.clone(),
            },
            old_session,
        )
    }

    pub async fn apply(&mut self) -> Result<ApplyReport, RuntimeError> {
        let next_session = self.connect_desired_session().await?;
        let (report, old_session) = self.install_connected_session(next_session);
        if let Some(old_session) = old_session {
            old_session.close().await?;
        }
        Ok(report)
    }

    pub async fn send_raw(&self, message: ClientMessage) -> Result<(), RuntimeError> {
        let session = self.session.as_ref().ok_or(RuntimeError::NotConnected)?;
        session.send_raw(message).await?;
        Ok(())
    }

    pub async fn next_server_event(&mut self) -> Option<ServerEvent> {
        let session = self.session.as_mut()?;
        session.next_event().await
    }

    pub async fn close(&mut self) -> Result<(), RuntimeError> {
        if let Some(session) = self.session.take() {
            session.close().await?;
        }
        Ok(())
    }

    fn build_session_config(&self, setup: SetupConfig) -> SessionConfig {
        let mut config = self.config.session.clone();
        config.setup = setup;
        config
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures_util::future::BoxFuture;
    use gemini_live::transport::TransportConfig;
    use gemini_live::types::{ClientMessage, Content, GoogleSearchTool, Part, SetupConfig, Tool};
    use gemini_live::{ReconnectPolicy, SessionError, SessionStatus};

    use super::*;
    use crate::config::{Patch, RuntimeConfig, SetupPatch};
    use crate::driver::SessionDriver;

    #[derive(Clone, Default)]
    struct FakeDriver {
        connects: Arc<Mutex<Vec<SessionConfig>>>,
        close_count: Arc<Mutex<usize>>,
    }

    #[derive(Clone)]
    struct FakeSession {
        close_count: Arc<Mutex<usize>>,
    }

    impl FakeDriver {
        fn connected_setups(&self) -> Vec<SetupConfig> {
            self.connects
                .lock()
                .expect("connects lock")
                .iter()
                .map(|config| config.setup.clone())
                .collect()
        }

        fn close_count(&self) -> usize {
            *self.close_count.lock().expect("close count lock")
        }
    }

    impl SessionDriver for FakeDriver {
        type Session = FakeSession;

        fn connect<'a>(
            &'a self,
            config: SessionConfig,
        ) -> BoxFuture<'a, Result<Self::Session, SessionError>> {
            let connects = Arc::clone(&self.connects);
            let close_count = Arc::clone(&self.close_count);
            Box::pin(async move {
                connects.lock().expect("connects lock").push(config);
                Ok(FakeSession { close_count })
            })
        }
    }

    impl RuntimeSession for FakeSession {
        fn status(&self) -> SessionStatus {
            SessionStatus::Connected
        }

        fn send_raw<'a>(
            &'a self,
            _message: ClientMessage,
        ) -> BoxFuture<'a, Result<(), SessionError>> {
            Box::pin(async { Ok(()) })
        }

        fn next_event<'a>(&'a mut self) -> BoxFuture<'a, Option<ServerEvent>> {
            Box::pin(async { None })
        }

        fn close(self) -> BoxFuture<'static, Result<(), SessionError>>
        where
            Self: Sized,
        {
            let close_count = Arc::clone(&self.close_count);
            Box::pin(async move {
                let mut value = close_count.lock().expect("close count lock");
                *value += 1;
                Ok(())
            })
        }
    }

    #[test]
    fn setup_patch_updates_selected_fields() {
        let mut setup = SetupConfig {
            model: "models/gemini-3.1-flash-live-preview".into(),
            system_instruction: Some(text_content("old")),
            tools: Some(vec![Tool::GoogleSearch(GoogleSearchTool {})]),
            ..Default::default()
        };
        let patch = SetupPatch {
            system_instruction: Patch::Set(text_content("new")),
            tools: Patch::Clear,
            ..Default::default()
        };

        patch.apply_to(&mut setup);

        assert_eq!(setup.system_instruction, Some(text_content("new")));
        assert_eq!(setup.tools, None);
    }

    #[tokio::test]
    async fn apply_reconnects_with_staged_setup() {
        let driver = FakeDriver::default();
        let mut runtime = LiveRuntime::new(
            RuntimeConfig {
                session: SessionConfig {
                    transport: TransportConfig::default(),
                    setup: SetupConfig {
                        model: "models/gemini-3.1-flash-live-preview".into(),
                        ..Default::default()
                    },
                    reconnect: ReconnectPolicy::default(),
                },
            },
            driver.clone(),
        );

        runtime.connect().await.expect("initial connect");
        runtime.stage_patch(&SetupPatch {
            system_instruction: Patch::Set(text_content("next")),
            ..Default::default()
        });

        let report = runtime.apply().await.expect("apply staged setup");
        let connected = driver.connected_setups();

        assert_eq!(connected.len(), 2);
        assert_eq!(connected[1].system_instruction, Some(text_content("next")));
        assert_eq!(report.previous_setup.system_instruction, None);
        assert_eq!(
            report.active_setup.system_instruction,
            Some(text_content("next"))
        );
        assert_eq!(
            runtime.active_setup().system_instruction,
            Some(text_content("next"))
        );
        assert_eq!(driver.close_count(), 1);
    }

    fn text_content(text: &str) -> Content {
        Content {
            role: None,
            parts: vec![Part {
                text: Some(text.to_string()),
                inline_data: None,
            }],
        }
    }
}
