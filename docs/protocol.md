# Gemini Multimodal Live API — Protocol Reference

> Upstream protocol facts. Update this file when the official API changes.
> Type definitions and field semantics live in the `types/` module doc comments.

---

## Official References

> As of 2026-04-05, the Gemini API Live API docs are still marked
> **Preview**. Re-audit this table whenever model support, auth flows, or
> tool semantics change.

| Resource | URL |
|---|---|
| **WebSockets API reference (primary)** | https://ai.google.dev/api/live |
| **Raw WebSocket quickstart** | https://ai.google.dev/gemini-api/docs/live-api/get-started-websocket |
| **Capabilities guide** | https://ai.google.dev/gemini-api/docs/live-api/capabilities |
| **Tool use guide** | https://ai.google.dev/gemini-api/docs/live-api/tools |
| **Session management** | https://ai.google.dev/gemini-api/docs/live-api/session-management |
| **Ephemeral tokens** | https://ai.google.dev/gemini-api/docs/live-api/ephemeral-tokens |
| **Deprecations / model lifecycle** | https://ai.google.dev/gemini-api/docs/deprecations |
| **Vertex AI Live API reference** | https://docs.cloud.google.com/vertex-ai/generative-ai/docs/model-reference/multimodal-live |

---

## Endpoints

```
# Standard (API key)
wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent?key={API_KEY}

# Ephemeral token (v1alpha)
wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1alpha.GenerativeService.BidiGenerateContentConstrained?access_token={TOKEN}
```

---

## Current Gemini API Live Models

The public Gemini API deprecations page currently lists these Live models:

| Model | Release date | Lifecycle note |
|---|---|---|
| `gemini-3.1-flash-live-preview` | 2026-03-11 | Current preview baseline |
| `gemini-2.5-flash-native-audio-preview-12-2025` | 2025-12-12 | Current preview model |
| `gemini-live-2.5-flash-preview` | 2025-06-17 | Deprecated; shutdown date 2025-12-09 |
| `gemini-2.0-flash-live-001` | 2025-04-09 | Deprecated; shutdown date 2025-12-09 |

When choosing defaults, prefer a model that still appears on the
deprecations page with no shutdown date announced.

---

## Protocol Lifecycle

```
Client                                    Server
  │                                         │
  │──── setup ─────────────────────────────▶│
  │◀─── setupComplete ─────────────────────│
  │                                         │
  │──── realtimeInput (audio/video/text) ──▶│
  │──── clientContent (history/turns) ─────▶│
  │◀─── serverContent (model turn) ────────│
  │◀─── serverContent (transcription) ─────│
  │◀─── sessionResumptionUpdate ───────────│
  │                                         │
  │◀─── toolCall ──────────────────────────│
  │──── toolResponse ──────────────────────▶│
  │◀─── toolCallCancellation ──────────────│
  │                                         │
  │◀─── goAway { timeLeft } ───────────────│
  │     (client should reconnect)           │
  │──── [close] ───────────────────────────▶│
```

---

## Source of Truth

Message-kind semantics, VAD settings, turn coverage, transcription flags, and
tool-field meanings should be read from the adjacent source comments in:

- `crates/gemini-live/src/types/client_message.rs`
- `crates/gemini-live/src/types/config.rs`
- `crates/gemini-live/src/types/server_message.rs`

This file intentionally keeps only the cross-cutting facts that do not have a
better natural home in one specific Rust type.

---

## Session Management

### Session Resumption

- Include `sessionResumption: {}` in setup to opt in.
- The server continuously sends fresh handles via `sessionResumptionUpdate`.
- Canonical field semantics live on `SessionResumptionConfig` and in
  `session.rs`.

### Context Window Compression

- `triggerTokens`: threshold that triggers compression (default ≈ 80% of context limit)
- `slidingWindow.targetTokens`: target size after compression
- Executed server-side automatically

### Session Duration Limits

| Scenario | Max duration |
|---|---|
| Audio only | 15 minutes (without compression) |
| Audio + video | 2 minutes (without compression) |
| WebSocket connection lifetime | Around 10 minutes |
| With context window compression | Session can be extended for an unlimited amount of time |
| Context window | Native audio models: 128k tokens; others: 32k tokens |

---

## Gemini 3.1 vs 2.5 Differences

| Feature | Gemini 3.1 | Gemini 2.5 |
|---|---|---|
| Thinking config | `thinkingLevel` (enum) | `thinkingBudget` (token count) |
| `clientContent` | Initial history only (via `historyConfig`) | Can be sent throughout the session |
| Response parts | One server event may contain multiple content parts | One content part per server event |
| Function calling | Supported, but synchronous only | Supported; async requires `behavior=NON_BLOCKING` |
| Function response scheduling | Not supported | `scheduling` is set inside `FunctionResponse.response` (`INTERRUPT` / `WHEN_IDLE` / `SILENT`) |
| Google Search tool | Supported | Supported |
| Proactive audio | Not supported | Supported (`proactivity.proactiveAudio`, `v1alpha`) |
| Affective dialogue | Not supported | Supported (`enableAffectiveDialog`, `v1alpha`) |
| Turn coverage default | `TURN_INCLUDES_AUDIO_ACTIVITY_AND_ALL_VIDEO` | `TURN_INCLUDES_ONLY_ACTIVITY` |

---

## Maintenance Notes

- Built-in Live tools are no longer limited to function calling. Search
  support should be tracked separately from custom tool declarations.
- The official tool docs place `behavior=NON_BLOCKING` on function
  declarations, but `scheduling` belongs inside
  `FunctionResponse.response`.
- Live model names and shutdown dates have already changed materially across
  2025-2026. Treat model lifecycle drift as a protocol maintenance issue.
