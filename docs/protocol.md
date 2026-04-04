# Gemini Multimodal Live API — Protocol Reference

> Upstream protocol facts. Update this file when the official API changes.
> Type definitions and field semantics live in the `types/` module doc comments.

---

## Official References

| Resource | URL |
|---|---|
| **API Reference (primary)** | https://ai.google.dev/api/live |
| **WebSocket Getting Started** | https://ai.google.dev/gemini-api/docs/live-api/get-started-websocket |
| **Feature Overview** | https://ai.google.dev/gemini-api/docs/live-api/capabilities |
| **Session Management** | https://ai.google.dev/gemini-api/docs/live-session |
| **Tool Use** | https://ai.google.dev/gemini-api/docs/live-api/tools |
| **Vertex AI Version** | https://docs.cloud.google.com/vertex-ai/generative-ai/docs/model-reference/multimodal-live |

---

## Endpoints

```
# Standard (API key)
wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent?key={API_KEY}

# Ephemeral token (v1alpha)
wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1alpha.GenerativeService.BidiGenerateContentConstrained?access_token={TOKEN}
```

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

## Message Format Quick Reference

**Client → Server** (4 kinds, each carrying exactly one top-level field):

| Wire field | Description | Code |
|---|---|---|
| `setup` | First message; configures model, tools, VAD, etc. | `types::client_message::SetupConfig` |
| `clientContent` | Conversation history / incremental turns | `types::client_message::ClientContent` |
| `realtimeInput` | Streaming audio / video / text / VAD signals | `types::client_message::RealtimeInput` |
| `toolResponse` | Reply to a server-initiated function call | `types::client_message::ToolResponseMessage` |

**Server → Client** (flat struct; multiple fields may be present simultaneously):

| Wire field | Description | Code |
|---|---|---|
| `setupComplete` | Confirms setup was accepted | `ServerEvent::SetupComplete` |
| `serverContent` | Model response / transcription / interruption | `ServerEvent::ModelText` / `ModelAudio` / … |
| `toolCall` | Requests function execution | `ServerEvent::ToolCall` |
| `toolCallCancellation` | Cancels a previous call | `ServerEvent::ToolCallCancellation` |
| `goAway` | Server will disconnect soon; client should reconnect | `ServerEvent::GoAway` |
| `sessionResumptionUpdate` | Fresh resume handle | `ServerEvent::SessionResumption` |
| `usageMetadata` | Token usage stats | `ServerEvent::Usage` |
| `error` | Error | `ServerEvent::Error` |

> Full field definitions are in the `types/` module doc comments.

---

## VAD & Turn Management

### Automatic VAD (default)

| Parameter | Description | Default |
|---|---|---|
| `startOfSpeechSensitivity` | Speech onset detection sensitivity | HIGH |
| `endOfSpeechSensitivity` | Speech offset detection sensitivity | HIGH |
| `prefixPaddingMs` | Audio retained before detected speech onset | 0 |
| `silenceDurationMs` | Silence duration to mark speech as ended | server default |

### Manual VAD

Set `automaticActivityDetection.disabled = true`; the client sends `activityStart` / `activityEnd`.

### Activity Handling

| Value | Behaviour |
|---|---|
| `START_OF_ACTIVITY_INTERRUPTS` | User speech interrupts the model (default) |
| `NO_INTERRUPTION` | Model continues uninterrupted |

### Turn Coverage

| Value | Default for |
|---|---|
| `TURN_INCLUDES_ONLY_ACTIVITY` | Gemini 2.5 |
| `TURN_INCLUDES_ALL_INPUT` | — |
| `TURN_INCLUDES_AUDIO_ACTIVITY_AND_ALL_VIDEO` | Gemini 3.1 |

---

## Session Management

### Session Resumption

- Include `sessionResumption: {}` in setup to opt in
- The server continuously sends fresh handles via `sessionResumptionUpdate`
- Handles are valid for **2 hours** after disconnect; sessions can be resumed within **24 hours**

### Context Window Compression

- `triggerTokens`: threshold that triggers compression (default ≈ 80% of context limit)
- `slidingWindow.targetTokens`: target size after compression
- Executed server-side automatically

### Session Duration Limits

| Scenario | Max duration |
|---|---|
| Audio only | 15 minutes (without compression) |
| Audio + video | 2 minutes (without compression) |
| Context window | Native audio models: 128k tokens; others: 32k tokens |

---

## Gemini 3.1 vs 2.5 Differences

| Feature | Gemini 3.1 | Gemini 2.5 |
|---|---|---|
| Thinking config | `thinkingLevel` (enum) | `thinkingBudget` (token count) |
| clientContent | Initial history only (via `historyConfig`) | Can be sent throughout the session |
| Function calling | Sequential only | Supports non-blocking + scheduling |
| Proactive audio | Not supported | Supported (v1alpha) |
| Turn coverage default | `AUDIO_ACTIVITY_AND_ALL_VIDEO` | `ONLY_ACTIVITY` |
| Response parts | Multiple parts per event | One part per event |
