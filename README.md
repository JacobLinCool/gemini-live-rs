# gemini-live-rs

High-performance Rust client for the [Gemini Multimodal Live API](https://ai.google.dev/api/live) — real-time, bidirectional audio/video/text streaming over WebSocket.

## Features

- **Strongly typed** — every wire message has a Rust struct; serde handles the JSON mapping
- **Session management** — automatic reconnection with exponential backoff, session resumption, GoAway handling
- **Streaming-first** — `send_audio` / `send_video` / `send_text` for real-time input; event stream for output
- **Performance-conscious** — zero-allocation `AudioEncoder` for the hot path; buffer-reuse design throughout
- **Tool calling** — built-in support for function calls, cancellations, and scheduling modes
- **Clone-friendly sessions** — `Session` is cheaply cloneable; multiple tasks can send and receive concurrently

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
gemini-live = { git = "https://github.com/jacoblincool/gemini-live-rs" }
tokio = { version = "1", features = ["full"] }
```

```rust
use gemini_live::session::{Session, SessionConfig, ReconnectPolicy};
use gemini_live::transport::{Auth, TransportConfig};
use gemini_live::types::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut session = Session::connect(SessionConfig {
        transport: TransportConfig {
            auth: Auth::ApiKey(std::env::var("GEMINI_API_KEY")?),
            ..Default::default()
        },
        setup: SetupConfig {
            model: "models/gemini-3.1-flash-live-preview".into(),
            generation_config: Some(GenerationConfig {
                response_modalities: Some(vec![Modality::Text]),
                ..Default::default()
            }),
            ..Default::default()
        },
        reconnect: ReconnectPolicy::default(),
    }).await?;

    session.send_text("Hello!").await?;

    while let Some(event) = session.next_event().await {
        match event {
            ServerEvent::ModelText(text) => print!("{text}"),
            ServerEvent::TurnComplete => println!("\n--- turn done ---"),
            _ => {}
        }
    }
    Ok(())
}
```

## Architecture

```
Session  →  Transport  →  Codec  →  Types / Audio / Errors
```

| Layer | Module | What it does |
|-------|--------|--------------|
| **Session** | `session.rs` | Connection lifecycle, auto-reconnect, typed send/receive |
| **Transport** | `transport.rs` | WebSocket + rustls, frame I/O |
| **Codec** | `codec.rs` | JSON ↔ Rust conversion; `ServerMessage` → `ServerEvent` decomposition |
| **Audio** | `audio.rs` | Zero-allocation PCM encoder, format constants |
| **Types** | `types/` | All wire-format structs and enums |
| **Errors** | `error.rs` | Layered error types per architectural layer |

Each layer's public API and design notes are documented in source code doc comments — start from `lib.rs` and drill into modules.

## Audio Streaming

For convenience:

```rust
session.send_audio(&pcm_i16_le_bytes).await?;
```

For maximum performance (zero allocation on the hot path):

```rust
let mut enc = AudioEncoder::new();
loop {
    let b64 = enc.encode_i16_le(&pcm_chunk);
    let msg = ClientMessage::RealtimeInput(RealtimeInput {
        audio: Some(Blob { data: b64.to_owned(), mime_type: "audio/pcm;rate=16000".into() }),
        video: None, text: None, activity_start: None, activity_end: None,
        audio_stream_end: None,
    });
    session.send_raw(msg).await?;
}
```

## Tool Calling

```rust
while let Some(event) = session.next_event().await {
    if let ServerEvent::ToolCall(calls) = event {
        let responses = calls.iter().map(|call| {
            let result = handle_function(&call.name, &call.args);
            FunctionResponse {
                id: call.id.clone(),
                name: call.name.clone(),
                response: result,
            }
        }).collect();
        session.send_tool_response(responses).await?;
    }
}
```

## CLI

An interactive text-mode CLI is included for quick testing:

```bash
GEMINI_API_KEY=your-key cargo run -p gemini-live-cli
```

Override the model with `GEMINI_MODEL`:

```bash
GEMINI_MODEL=models/gemini-3.1-flash-live-preview cargo run -p gemini-live-cli
```

## Documentation

| File | Purpose |
|------|---------|
| `docs/protocol.md` | Upstream API reference (endpoints, lifecycle, VAD, session limits, model differences) |
| `docs/design.md` | Architecture decisions and performance goals |
| `docs/roadmap.md` | Planned work, known gaps, tech debt |
| `docs/testing.md` | Test inventory and instructions |

## License

MIT

---

## Author's Note

This repository is also an experiment in **how to design a set of guiding principles that enable AI agents to autonomously maintain a client library over time**.

Maintaining a client library is not a one-shot code generation problem — it is an ongoing engineering challenge. The library must track upstream API changes, keep documentation in sync, preserve backward compatibility, expand test coverage, and maintain design consistency. These are exactly the kinds of tasks where AI agents could contribute meaningfully, if given the right structure to work within.

The core idea behind this project is to explore what that structure looks like: which conventions, workflows, and constraints help an AI agent maintain stable, extensible, and high-quality output with minimal human intervention. The documentation architecture here — `AGENTS.md` for general principles, `protocol.md` for upstream facts, `design.md` for our decisions, `roadmap.md` for tracking gaps — is designed so that an agent can orient itself, identify what needs to change, and act accordingly.

If these principles can be defined clearly enough, an AI agent becomes more than a tool that executes instructions — it becomes a collaborator capable of participating in long-term maintenance.
