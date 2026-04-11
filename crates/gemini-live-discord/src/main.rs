use gemini_live_discord::{DiscordAgentService, DiscordBotConfig};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("gemini_live_discord=info,gemini_live_runtime=info,gemini_live=info")
    });
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let config = DiscordBotConfig::from_env()?;
    if let Err(error) = DiscordAgentService::new(config).prepare()?.run().await {
        eprintln!("Error: {error}");
        if let Some(hint) = error.startup_hint() {
            eprintln!("{hint}");
        }
        return Err(error.into());
    }
    Ok(())
}
