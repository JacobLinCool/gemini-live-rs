mod media;

use std::io::Write;

use base64::Engine;
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
                response_modalities: Some(vec![Modality::Audio]),
                media_resolution: Some(MediaResolution::MediaResolutionLow),
                ..Default::default()
            }),
            output_audio_transcription: Some(AudioTranscriptionConfig {}),
            ..Default::default()
        },
        reconnect: ReconnectPolicy::default(),
    })
    .await?;

    eprintln!("connected — type a message and press Enter (Ctrl-D to quit)");
    eprintln!("  use @file.jpg / @file.wav to send images or audio\n");

    // Event printer task
    let mut recv = session.clone();
    let printer = tokio::spawn(async move {
        while let Some(event) = recv.next_event().await {
            match event {
                ServerEvent::OutputTranscription(text) => {
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

        if let Err(e) = send_input(&session, trimmed).await {
            eprintln!("[send error] {e}");
            break;
        }
    }

    session.close().await?;
    let _ = printer.await;
    Ok(())
}

async fn send_input(session: &Session, line: &str) -> Result<(), Box<dyn std::error::Error>> {
    let (text, file_paths) = media::parse_input(line);

    // Send media files first
    for path in &file_paths {
        match media::load(path) {
            Ok(m) => {
                eprintln!("  {}", media::describe(path, &m));
                match m {
                    media::Media::Image { data, mime } => {
                        session.send_video(&data, mime).await?;
                    }
                    media::Media::Audio { pcm, sample_rate } => {
                        send_audio_with_rate(session, &pcm, sample_rate).await?;
                    }
                }
            }
            Err(e) => {
                eprintln!("  [skip] @{path}: {e}");
            }
        }
    }

    // If we sent media, give the model a moment to process it before sending text.
    if !file_paths.is_empty() && !text.is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    // Send text if any
    if !text.is_empty() {
        session.send_text(&text).await?;
    }

    Ok(())
}

async fn send_audio_with_rate(
    session: &Session,
    pcm: &[u8],
    sample_rate: u32,
) -> Result<(), gemini_live::SessionError> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(pcm);
    let mime = format!("audio/pcm;rate={sample_rate}");
    session
        .send_raw(ClientMessage::RealtimeInput(RealtimeInput {
            audio: Some(Blob {
                data: b64,
                mime_type: mime,
            }),
            video: None,
            text: None,
            activity_start: None,
            activity_end: None,
            audio_stream_end: None,
        }))
        .await
}
