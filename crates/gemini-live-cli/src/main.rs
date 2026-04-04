use std::io::Write;

use gemini_live::session::{ReconnectPolicy, Session, SessionConfig};
use gemini_live::transport::{Auth, TransportConfig};
use gemini_live::types::*;
use tokio::io::AsyncBufReadExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let api_key = std::env::var("GEMINI_API_KEY").expect("set GEMINI_API_KEY environment variable");

    let model = std::env::var("GEMINI_MODEL")
        .unwrap_or_else(|_| "models/gemini-3.1-flash-live-preview".into());

    eprintln!("connecting to {model}…");

    let session = Session::connect(SessionConfig {
        transport: TransportConfig {
            auth: Auth::ApiKey(api_key),
            ..Default::default()
        },
        setup: SetupConfig {
            model,
            generation_config: Some(GenerationConfig {
                response_modalities: Some(vec![Modality::Text]),
                ..Default::default()
            }),
            ..Default::default()
        },
        reconnect: ReconnectPolicy::default(),
    })
    .await?;

    eprintln!("connected — type a message and press Enter (Ctrl-D to quit)\n");

    // Event printer task
    let mut recv = session.clone();
    let printer = tokio::spawn(async move {
        while let Some(event) = recv.next_event().await {
            match event {
                ServerEvent::ModelText(text) => {
                    print!("{text}");
                    std::io::stdout().flush().ok();
                }
                ServerEvent::TurnComplete => {
                    println!("\n");
                    eprint!("> ");
                    std::io::stderr().flush().ok();
                }
                ServerEvent::Error(e) => eprintln!("\n[error] {}", e.message),
                ServerEvent::Closed { reason } if !reason.is_empty() => {
                    eprintln!("\n[closed] {reason}");
                    break;
                }
                _ => {}
            }
        }
    });

    // Read lines from stdin
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    eprint!("> ");
    std::io::stderr().flush().ok();

    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            eprint!("> ");
            std::io::stderr().flush().ok();
            continue;
        }
        if let Err(e) = session.send_text(trimmed).await {
            eprintln!("[send error] {e}");
            break;
        }
    }

    session.close().await?;
    let _ = printer.await;
    Ok(())
}
