# Roadmap & Tech Debt

> Tracks everything we've identified but haven't done yet.
> Items move to "Done" (with a date) or get removed when no longer relevant.
> This is the single source of truth for prioritisation — don't scatter TODOs elsewhere.

---

## Performance

There are currently no open protocol-layer performance items. New entries
should be added here only when a measured regression or newly identified
structural inefficiency is still unresolved.

## Done

| ID | Item | Date | Notes |
|----|------|------|-------|
| P-3 | `codec::into_events` audio decode allocation | 2026-04-12 | Split server-side model turns away from the client `Blob` type and decode inline audio during deserialization into `Bytes`. This removes one owned base64-audio buffer from the recv path; pure decomposition work is much cheaper, while end-to-end recv latency stays roughly flat because the decode work now happens earlier in `codec::decode`. |
| P-1 | `send_audio` / `send_video` per-call allocation | 2026-04-12 | Moved audio/video convenience sends onto dedicated runner commands with reusable base64 / MIME / JSON buffers, eliminating the per-chunk base64 `String` allocation from `send_audio` and the common JPEG/PNG MIME allocation from `send_video`. |
| P-2 | `codec::encode` per-call allocation | 2026-04-12 | Added `codec::encode_into(&mut Vec<u8>)` and moved the session runner plus setup handshake onto reusable JSON buffers, removing the `serde_json::to_string` allocation from the hot send path. |
| P-5 | Benchmark baselines and idle regression coverage | 2026-04-12 | Added checked-in Criterion baselines for `gemini-live-runtime` and `gemini-live-harness`, plus deterministic regression tests for idle timeout boundaries and signal-driven notification wake behavior. |
| P-6 | Desktop audio callback allocations | 2026-04-12 | Reworked `gemini-live-io` mic/speaker adapters around reusable scratch buffers and in-place resampling, eliminating steady-state callback allocations on the speaker path and removing most transient mic callback heap traffic. |
| P-4 | `ServerEvent::ModelAudio` clone cost | 2026-04-12 | Switched model-audio events to `bytes::Bytes`, so runtime fanout now clones payload handles instead of copying PCM buffers. |
| F-10 | Harness host lifecycle parity | 2026-04-12 | The CLI now uses `SessionManager` like Discord does, so wake / resume / dormant transitions and passive-notification wake semantics no longer depend on a permanently hot desktop session. |

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
| F-12 | Harness notification source metadata | Durable notifications still do not record structured origin metadata such as `source_kind`, `source_id`, or a future dedupe key. Add at least source identity fields so prompt formatting, debugging, and later de-duplication do not rely on free-form notification text alone. | Medium |

## CLI Product

Work needed to turn `gemini-live-cli` from a good demo into a dependable
end-user application.

| ID | Item | Description | Priority |
|----|------|-------------|----------|
| C-1 | Richer session profiles | Persistent named profiles now cover backend, model, system instruction, credentials, tools, and device auto-start state. Extend them to first-class voice and richer session-template controls. | High |
| C-4 | Distribution truthfulness | `update.rs` advertises Linux ARM64, but the release workflow does not ship that artifact. Align updater targets with published binaries. | Medium |
| C-5 | Profile management surface | Harness-managed named profiles now back CLI and Discord state, but there is still no explicit host-facing surface for listing, renaming, deleting, or copying profiles. Add that only after notification metadata and host lifecycle semantics are in better shape, so we do not widen the product surface before the underlying recovery model is solid. | Low |

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
| T-7 | CLI parser / reducer / tool-runtime tests | Slash parser/completion coverage, startup/render tests, tool-catalog tests, app-reducer tests, outbound send tests, and managed-runtime tests now exist. Expand coverage to staged-profile apply flow and richer local tool execution boundaries. | High |
| T-8 | Discord host tests | The Discord crate now covers config parsing, routing policy, target-channel planning, runtime bootstrap, service helper behavior, and the current text/voice reply projection semantics. Add higher-level tests for guild setup execution against mocked Discord HTTP, runtime event projection, and Songbird bridge lifecycle. | High |

## Tech Debt

Code quality issues that aren't bugs but should be cleaned up.

| ID | Item | Description | Severity |
|----|------|-------------|----------|
| D-2 | Error granularity on connect | `ConnectError::Dns` and `ConnectError::Tls` variants exist but are never constructed — all non-HTTP errors fall into `ConnectError::Ws`. Add classification logic or remove dead variants. | Low |
| D-3 | Library/CLI distribution mismatch | The CLI's self-update path, release workflow, and install script need to be kept in sync. Today the updater advertises at least one target that the release workflow does not build. | Medium |
