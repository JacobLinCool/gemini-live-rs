# Architecture & Design Decisions

> Our design choices. Update this file when refactoring the architecture.
> ADRs are append-only — mark outdated ones with `[superseded]`.

---

## Performance Goal

This crate targets **maximum performance** — minimise latency and allocations without compromising correctness.

Real-time audio/video streaming is extremely latency-sensitive: every extra heap allocation and every unnecessary copy adds directly to user-perceived latency. Design decisions should prioritise zero-allocation / zero-copy on the hot path (the code path every audio chunk traverses).

### Hot Path Definition

The following paths execute every **100–250 ms** during 16 kHz audio streaming:

1. **Send**: raw PCM → base64 encode → assemble `RealtimeInput` JSON → WebSocket send
2. **Recv**: WebSocket recv → JSON parse → base64 decode audio → broadcast event

### Performance Principles

| Principle | Description |
|-----------|-------------|
| **Buffer reuse** | Encoders / serializers on the hot path use pre-allocated buffers to avoid per-chunk allocation |
| **Zero-copy first** | Borrow (`&str` / `&[u8]`) whenever possible; only take ownership at task boundaries |
| **Batch-friendly** | API design allows callers to reuse a single encoder instance across a streaming loop |
| **Lazy conversion** | Don't perform conversions the caller didn't ask for (e.g. PCM bytes go straight to base64 — no forced f32 round-trip) |
| **Profile, don't guess** | Optimisations must be backed by benchmark data, not intuition |

### Known Performance Gaps

Tracked in [`roadmap.md`](roadmap.md) items **P-1** through **P-5**.

---

## Layer Architecture

```
┌──────────────────────────────────────────────────────────────┐
│              Host Applications (`cli` / `discord`)           │
├──────────────────────────────────────────────────────────────┤
│  Desktop Media Adapters (`gemini-live-io`)                   │
│  Mic capture · speaker playback · screen capture             │
├──────────────────────────────────────────────────────────────┤
│  Runtime Layer (`gemini-live-runtime`)                       │
│  Staged setup · driver boundary · reusable host contracts    │
├──────────────────────────────────────────────────────────────┤
│  Session Layer (session.rs)                                  │
│  Connection lifecycle · auto-reconnect · session resumption  │
├──────────────────────────────────────────────────────────────┤
│  Codec Layer (codec.rs)                                      │
│  ServerMessage ↔ ServerEvent decomposition · encode / decode │
├──────────────────────────────────────────────────────────────┤
│  Transport Layer (transport.rs)                              │
│  WebSocket + rustls · frame I/O · no JSON awareness          │
├──────────────────────────────────────────────────────────────┤
│  Types (types/) + Audio (audio.rs) + Errors (error.rs)       │
└──────────────────────────────────────────────────────────────┘
```

Each layer's public surface and design notes live in the corresponding source module / struct doc comments.

---

## Design Decision Records

### ADR-1: `ServerMessage` is a flat struct, not an enum

The protocol allows the server to include multiple top-level fields in a single JSON message (e.g. `serverContent` + `usageMetadata`). A flat struct with `Option` fields preserves this; `codec::into_events()` then decomposes it into semantically clear `ServerEvent` enum variants for the caller.

### ADR-2: `ClientMessage` is an externally-tagged enum

The protocol requires each client message to carry exactly one top-level field. Serde's externally-tagged enum (`#[serde(rename = "setup")]`) naturally produces `{"setup": { ... }}` — no manual `Serialize` impl needed.

### ADR-3: Reconnection strategy

- Exponential backoff: `base × 2^(attempt − 1)`, capped at `max_backoff`
- GoAway → reconnect immediately (carrying the latest resume handle)
- Connection loss → apply backoff
- On reconnect, `sessionResumption.handle` is injected into the setup message automatically
- Transparent to the caller: sends buffer in the mpsc channel during downtime

### ADR-4: Session handle is Clone

Multiple async tasks often need to hold the same session simultaneously (one sending audio, another receiving events). Using `Arc` + `broadcast::Sender` / `mpsc::Sender` (both Clone) makes `Session` cheaply cloneable. Each clone has its own event cursor (its own `broadcast::Receiver`).

### ADR-5: Audio encoder buffer reuse

`AudioEncoder` writes the PCM → base64 conversion into pre-allocated internal buffers and returns a borrowed `&str`. Callers reuse the same `AudioEncoder` instance in a loop, producing **no heap allocations** on the hot path.

**Note:** the current `Session::send_audio` convenience method does **not** use `AudioEncoder` — it calls `STANDARD.encode()` which allocates a new `String` each time. Callers needing maximum performance should use `AudioEncoder` + `Session::send_raw` directly. This is a known gap (see [`roadmap.md`](roadmap.md) **P-1**).

### ADR-6: CLI setup changes are staged, then applied via explicit reconnect

Live API tools are part of the immutable `setup` payload for an open
connection. The CLI therefore separates:

- **desired tool profile** — changed by `/tools enable|disable|toggle`
- **active tool profile** — the profile on the currently connected session

`/tools apply` and `/system apply` promote staged setup changes by reconnecting
with a newly built `setup` payload. This keeps the command surface honest:
toggling a tool does not pretend to mutate a connection that the protocol says
is already configured.

The current implementation uses explicit session resumption when a
server-issued resumption handle is available, so conversation state can carry
over across the reconnect. If the server has not published a handle yet, the
apply fails explicitly instead of silently falling back to a fresh session.

### ADR-7: CLI user preferences live in a global named-profile store

The CLI now persists user-facing configuration in a global TOML file keyed by
profile name. A profile captures:

- backend selection and credentials
- model selection
- system instruction
- staged tool profile
- microphone / speaker auto-start flags
- optional screen-share startup settings

Startup resolution is `environment > active profile > built-in defaults`. The
resolved values are then written back to the active profile so one explicit run
can seed future launches.

This design keeps startup deterministic and easy to explain: the active
profile is the durable source of truth, while environment variables remain a
simple override layer for automation or one-off launches.

### ADR-8: Reusable host orchestration lives outside the CLI binary

The workspace now has a dedicated `gemini-live-runtime` crate for the layer
that sits above the base `gemini-live` session client and below concrete host
applications such as the desktop CLI or a future voice bot.

That crate is the natural home for:

- staged-vs-active `setup` orchestration
- a testable session-driver abstraction above `Session`
- shared runtime event and tool-execution contracts
- managed session forwarding and tool-call orchestration (`ManagedRuntime`)

This avoids turning the CLI into the accidental source of truth for reusable
application logic, and it avoids creating a vague "utils" crate with no
clear architectural boundary.

### ADR-9: Desktop media adapters live in `gemini-live-io`

Desktop-specific mic, speaker, and screen-capture code now lives in a
dedicated `gemini-live-io` crate instead of the CLI binary.

That crate is the natural home for:

- microphone capture and AEC-backed cleaned PCM output
- speaker playback and AEC render-reference feeding
- screen-target enumeration and JPEG frame capture

This keeps the CLI focused on TUI product behavior and lets future desktop
hosts reuse the same media adapters without reaching into a binary crate.

### ADR-10: Discord host behavior lives in `gemini-live-discord`

The workspace now also has a dedicated `gemini-live-discord` crate for the
Discord-specific host layer that pairs `serenity` and `songbird` with the
shared Gemini runtime.

That crate is the natural home for:

- single-guild / single-owner routing policy
- configured target voice-channel discovery and creation
- Discord gateway intents and event handling
- the Songbird voice receive / playback bridge

This keeps Discord product logic out of the desktop CLI and avoids trying to
force Discord voice handling through the desktop-only `gemini-live-io` crate.
