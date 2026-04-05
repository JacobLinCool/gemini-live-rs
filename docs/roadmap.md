# Roadmap & Tech Debt

> Tracks everything we've identified but haven't done yet.
> Items move to "Done" (with a date) or get removed when no longer relevant.
> This is the single source of truth for prioritisation — don't scatter TODOs elsewhere.

---

## Performance

Items from the "maximum performance" goal (see `design.md`).

| ID | Item | Description | Severity |
|----|------|-------------|----------|
| P-1 | `send_audio` / `send_video` per-call allocation | `Session::send_audio` calls `STANDARD.encode()` allocating a new `String` every chunk instead of reusing `AudioEncoder`'s buffer. Workaround: use `AudioEncoder` + `send_raw`. | Medium |
| P-2 | `codec::encode` per-call allocation | `serde_json::to_string` allocates a new `String` per call. Should offer `encode_into(&mut Vec<u8>)` writing into a reusable buffer. | Medium |
| P-3 | `codec::into_events` audio decode allocation | base64 decoding audio data allocates a new `Vec<u8>`. Consider decode-in-place or caller-provided buffer. | Low |
| P-4 | `ServerEvent::ModelAudio` clone cost | Holds `Vec<u8>`; broadcast channel clones the entire buffer on send. Switch to `bytes::Bytes` (ref-counted, O(1) clone). | Medium |
| P-5 | Benchmarks | No benchmarks exist yet. Add `criterion` benchmarks for the hot path: audio encode, codec encode/decode, base64 round-trip. Needed to validate all other perf work. | High |

## Features

Missing or incomplete functionality relative to the upstream API.

| ID | Item | Description | Priority |
|----|------|-------------|----------|
| F-1 | Ephemeral token fetching | `Auth::EphemeralToken` accepts a token string but there's no helper to obtain one from the REST API. Add an optional `reqwest`-based helper (behind a feature flag). `reqwest` is already in workspace deps. | Medium |
| F-2 | Vertex AI endpoint support | `endpoint_override` exists as an escape hatch, but there's no first-class `Auth::VertexAI` variant with service-account auth. | Low |
| F-3 | Configurable setup timeout | Setup handshake timeout is hardcoded to 30 s (`SETUP_TIMEOUT` in `session.rs`). Should be configurable via `SessionConfig`. | Low |
| F-4 | Graceful shutdown propagation | `Session::close()` sends a close command but doesn't await the runner task to finish. Consider returning a `JoinHandle` or awaiting completion. | Low |
| F-5 | `Stream` trait for `Session` | `events()` returns `impl Stream` via `unfold`, but `Session` itself doesn't implement `Stream`. Evaluate whether implementing `Stream<Item = ServerEvent>` directly on `Session` would be ergonomic. | Low |
| F-6 | Audio output decoding | No counterpart to `AudioEncoder` for decoding received 24 kHz PCM audio (base64 decode + optional i16-to-f32 conversion). | Medium |
| F-7 | `send_audio` with custom sample rate | `Session::send_audio` hardcodes `audio/pcm;rate=16000`. Add `send_audio_at_rate(pcm, rate)` so callers with non-16kHz sources (mic, WAV files) don't need `send_raw`. CLI currently works around this. | Medium |

## Testing

Planned tests not yet implemented.

| ID | Item | Description | Priority |
|----|------|-------------|----------|
| T-1 | Integration: connect + setup handshake | Transport layer: connect → send setup → receive `setupComplete` against the real API. | High |
| T-2 | Integration: text round-trip | Session: `send_text` → receive `ModelText` + `TurnComplete`. | High |
| T-3 | Integration: tool calling | Session: receive `ToolCall` → `send_tool_response` → model continues. | Medium |
| T-4 | Integration: GoAway reconnect | Session: simulate or trigger `GoAway` → verify auto-reconnect with resume handle. | Medium |
| T-5 | E2E: multimodal streaming | Audio + video sent simultaneously; verify both are processed. | Low |
| T-6 | Stress: reconnection stability | Unstable network simulation → verify no events are dropped across reconnections. | Low |

## Tech Debt

Code quality issues that aren't bugs but should be cleaned up.

| ID | Item | Description | Severity |
|----|------|-------------|----------|
| D-1 | `RealtimeInput` construction verbosity | Every convenience method in `Session` manually sets all 6 `Option` fields to `None`. Should derive `Default` on `RealtimeInput` and use `..Default::default()`. | Low |
| D-2 | Error granularity on connect | `ConnectError::Dns` and `ConnectError::Tls` variants exist but are never constructed — all non-HTTP errors fall into `ConnectError::Ws`. Add classification logic or remove dead variants. | Low |
| D-3 | `reqwest` workspace dependency unused | `reqwest` is declared in workspace `Cargo.toml` but not used by any crate. Either implement F-1 (ephemeral token helper) or remove it. | Low |

