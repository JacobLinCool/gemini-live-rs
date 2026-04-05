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
| P-5 | Benchmark baselines and CI gating | Criterion benchmarks now exist in `crates/gemini-live/benches/hot_path.rs`, but there are no checked-in baselines, regression thresholds, or CI reporting. We still cannot prove whether perf changes help or hurt over time. | Medium |

## Features

Missing or incomplete functionality relative to the upstream API.

| ID | Item | Description | Priority |
|----|------|-------------|----------|
| F-1 | Ephemeral token fetching | `Auth::EphemeralToken` accepts a token string but there's no helper to obtain one from the REST API. Add an optional `reqwest`-based helper (behind a feature flag). `reqwest` is already in workspace deps. | Medium |
| F-2 | Vertex AI schema parity and integration coverage | First-class transport routing and ADC-backed bearer refresh now exist, but the Vertex `v1` schema audit is still incomplete and there are no live Vertex integration tests yet. | Medium |
| F-3 | Configurable setup timeout | Setup handshake timeout is hardcoded to 30 s (`SETUP_TIMEOUT` in `session.rs`). Should be configurable via `SessionConfig`. | Low |
| F-4 | Graceful shutdown propagation | `Session::close()` sends a close command but doesn't await the runner task to finish. Consider returning a `JoinHandle` or awaiting completion. | Low |
| F-5 | `Stream` trait for `Session` | `events()` returns `impl Stream` via `unfold`, but `Session` itself doesn't implement `Stream`. Evaluate whether implementing `Stream<Item = ServerEvent>` directly on `Session` would be ergonomic. | Low |
| F-6 | Audio output decoding | No counterpart to `AudioEncoder` for decoding received 24 kHz PCM audio (base64 decode + optional i16-to-f32 conversion). | Medium |
| F-7 | Broader built-in Live tools | Typed `googleSearch` setup coverage now exists. Extend the typed tool surface to the remaining built-in tools the Live API exposes on supported models/endpoints, instead of forcing callers into raw JSON. | Medium |
| F-8 | Gemini 2.5-only session features | `enableAffectiveDialog` and `proactivity.proactiveAudio` are explicit Live API capabilities on Gemini 2.5 (`v1alpha`). Audit and add missing typed coverage. | Medium |
| F-9 | Async function-calling wire semantics audit | Official docs put `behavior=NON_BLOCKING` on function declarations and `scheduling` inside `FunctionResponse.response`. Audit our types and examples against the current wire contract. | High |

## CLI Product

Work needed to turn `gemini-live-cli` from a good demo into a dependable
end-user application.

| ID | Item | Description | Priority |
|----|------|-------------|----------|
| C-1 | Richer session profiles | Persistent named profiles now cover backend, model, system instruction, credentials, tools, and device auto-start state. Extend them to first-class voice and richer session-template controls. | High |
| C-2 | Tool-profile resumption and context carryover | The CLI now executes local function calls and can enable Google Search, but `/tools apply` still uses a fresh session. Add an explicit resume / carryover path so tool-profile changes do not drop server-side conversation state. | High |
| C-3 | Runtime observability | Surface reconnecting, closed, lagged, and send-failure states in the TUI instead of swallowing `.ok()` results. | High |
| C-4 | Distribution truthfulness | `update.rs` advertises Linux ARM64, but the release workflow does not ship that artifact. Align updater targets with published binaries. | Medium |
| C-5 | Extract testable CLI boundaries | Split command parsing, event reduction, and render-state transitions out of `main.rs` so the CLI can gain unit and snapshot coverage. | High |

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
| T-7 | CLI parser / reducer / tool-runtime tests | Basic slash parser/completion coverage plus tool-catalog tests now exist. Expand coverage to server-event handling, staged-profile apply flow, media input edge cases, and local tool execution boundaries. | High |

## Tech Debt

Code quality issues that aren't bugs but should be cleaned up.

| ID | Item | Description | Severity |
|----|------|-------------|----------|
| D-2 | Error granularity on connect | `ConnectError::Dns` and `ConnectError::Tls` variants exist but are never constructed — all non-HTTP errors fall into `ConnectError::Ws`. Add classification logic or remove dead variants. | Low |
| D-3 | Library/CLI distribution mismatch | The CLI's self-update path, release workflow, and install script need to be kept in sync. Today the updater advertises at least one target that the release workflow does not build. | Medium |
