# gemini-live-cli

Interactive TUI client for the Gemini Multimodal Live API. Serves as both
a practical tool and a living usage example of the `gemini-live` library.

## Quick Start

```bash
GEMINI_API_KEY=your-key cargo run -p gemini-live-cli
```

Override the model:

```bash
GEMINI_MODEL=models/gemini-2.5-flash-native-audio-latest cargo run -p gemini-live-cli
```

Use Vertex AI with a static bearer token:

```bash
LIVE_BACKEND=vertex \
VERTEX_LOCATION=us-central1 \
VERTEX_MODEL='projects/PROJECT_ID/locations/us-central1/publishers/google/models/MODEL_ID' \
VERTEX_AI_ACCESS_TOKEN="$(gcloud auth application-default print-access-token)" \
cargo run -p gemini-live-cli
```

Use Vertex AI with Application Default Credentials:

```bash
LIVE_BACKEND=vertex \
VERTEX_LOCATION=us-central1 \
VERTEX_MODEL='projects/PROJECT_ID/locations/us-central1/publishers/google/models/MODEL_ID' \
VERTEX_AUTH=adc \
cargo run -p gemini-live-cli --features vertex-auth
```

Select or create a named profile:

```bash
cargo run -p gemini-live-cli -- --profile work
```

Print the resolved config file path:

```bash
cargo run -p gemini-live-cli -- config
```

## Profiles & Config

The CLI persists global configuration in:

- `$XDG_CONFIG_HOME/gemini-live/config.toml`
- or `~/.config/gemini-live/config.toml` when `XDG_CONFIG_HOME` is unset
- or `%APPDATA%\\gemini-live\\config.toml` on Windows

Profiles are keyed by name inside that file. If `--profile <name>` refers to a
missing profile, the CLI creates it automatically and saves resolved settings
into it.

The selected profile persists:

- backend selection and credentials
- model
- system instruction
- enabled tools
- microphone / speaker auto-start state
- optional screen-share target and interval

Startup precedence is:

1. Environment variables
2. Active profile values
3. Built-in defaults

Resolved startup settings are written back to the active profile, so exporting
`GEMINI_API_KEY` or `GEMINI_MODEL` for one run is enough to seed a profile for
later runs. This file may therefore contain plaintext credentials.

## Canonical Behavior

The canonical description of the default CLI session profile now lives in the
module docs for `crates/gemini-live-cli/src/main.rs`. Keep that source comment
in sync with the `SetupConfig` built by the CLI entrypoint.

Backend selection and auth-mode semantics are also documented in that module
doc and enforced by the startup config helpers in the same file.

## UI Layout

```
┌─ models/gemini-3.1-flash-live-preview ──────────────────┐
│   connected — @file for media, /mic /speak for audio    │
│ [you] hello                                             │
│ [model] 你好！有什麼我可以幫你的嗎？                       │
│   [image] photo.jpg (45.2 KB, image/jpeg)               │  ← system info
│ [model] I see a screenshot showing...                   │
│ [model] streaming response in gray...                   │  ← partial (gray)
├─ mic: ON | speak: off | screen: off ────────────────────┤
│ > your input here|                                      │
└─────────────────────────────────────────────────────────┘
```

- **Top panel**: conversation history, auto-scrolls to bottom
- **Bottom panel**: always-active input — you can type while the model is responding
- **Status bar**: current state of audio/screen features
- **Output transcript**: shown as model text when output audio transcription is enabled

## Commands

### Text & Media

| Input | Action |
|-------|--------|
| `hello` | Send text to the model |
| `@photo.jpg` | Send an image file |
| `@recording.wav` | Send a WAV audio file |
| `@photo.jpg describe this` | Send image + text together (image first, text after 500ms delay) |

Supported image formats: JPEG, PNG, GIF, WebP, BMP.
Supported audio formats: WAV (any sample rate, auto mono mixdown), raw PCM (.pcm/.raw, assumed 16kHz mono).

File paths are currently parsed by splitting on whitespace. Paths containing
spaces are not supported yet.

### Slash Commands

| Command | Action |
|---------|--------|
| `/mic` | Toggle microphone input. Captures from the default input device and streams to the API at the device's native sample rate. |
| `/speak` | Toggle speaker output. Plays model audio (24 kHz PCM) through the default output device, resampled to the device's native rate. When both mic and speak are on, WebRTC AEC (Acoustic Echo Cancellation) automatically removes speaker echo from the mic signal. |
| `/share-screen list` | List available capture targets (monitors and windows) with IDs. |
| `/share-screen <id> [interval]` | Start sharing a monitor or window. `interval` is seconds between frames (default: 1). Example: `/share-screen 0 5` shares Display 1 every 5 seconds. |
| `/share-screen` | Stop screen sharing (when active). |
| `/system` | Show active vs staged system instruction. |
| `/system show` | Show active vs staged system instruction. |
| `/system set <text>` | Stage a new system instruction for the next applied session. Quote the text when it contains spaces you want preserved literally. |
| `/system clear` | Stage removal of the system instruction. |
| `/system apply` | Open a fresh Live session using the staged system instruction and staged tools. |
| `/tools` | Show active vs staged tool profile. |
| `/tools list` | List the known tools and their current state (`active`, `staged`, `off`). |
| `/tools enable <tool>` | Stage a tool for the next applied session. Known tools: `google-search`, `list-files`, `read-file`, `run-command`. |
| `/tools disable <tool>` | Stage a tool removal for the next applied session. |
| `/tools toggle <tool>` | Flip a tool in the staged profile. |
| `/tools apply` | Open a fresh Live session using the staged tool profile. |

When the input starts with `/`, the CLI shows slash-command completions in a
popup above the input box. `Tab` accepts the selected completion and `Up` /
`Down` switch the highlighted suggestion.

### Keyboard

| Key | Action |
|-----|--------|
| `Enter` | Send input / execute command |
| `Tab` | Accept the highlighted slash-command completion |
| `Up` / `Down` | Move the slash-command completion selection (when visible) |
| `Backspace` | Delete last character |
| `Esc` / `Ctrl-C` / `Ctrl-D` | Quit |

## Feature Flags

Each slash command group is a separate Cargo feature, all enabled by default:

| Feature | Dependencies | Enables |
|---------|-------------|---------|
| `mic` | `cpal`, `webrtc-audio-processing` | `/mic` command with AEC |
| `speak` | `cpal`, `webrtc-audio-processing` | `/speak` command with AEC |
| `share-screen` | `xcap`, `image` | `/share-screen` command |
| `vertex-auth` | `gemini-live/vertex-auth` | `VERTEX_AUTH=adc` via Google Cloud Application Default Credentials |

Build without audio/screen support for a minimal binary:

```bash
cargo build -p gemini-live-cli --no-default-features
```

## Architecture

```
main.rs      TUI event loop (crossterm + ratatui), command dispatch
input.rs     Single-line editor wrapper built on `tui-textarea`
slash.rs     Structured slash-command parsing (`clap`) + completion model
media.rs     @file loading: image/audio detection, WAV decoding, mono mixdown
audio_io.rs  cpal mic capture + speaker playback (gated by mic/speak features)
screen.rs    xcap screen capture thread + JPEG encoding (gated by share-screen)
```

The main loop uses `tokio::select!` to concurrently poll:
1. Terminal key events (crossterm `EventStream`)
2. Session server events (via `mpsc` bridge)
3. Microphone PCM chunks (when mic is active)
4. Screen capture JPEG frames (when sharing)

This non-blocking design means the user can type, receive model responses, and
stream audio/video simultaneously without any of these blocking each other.

## Current Limitations

See `crates/gemini-live-cli/src/main.rs` for code-adjacent notes about the
default profile and current tool behavior. This file is intentionally kept as
an entry guide rather than the canonical home of runtime behavior.

- Tool-profile changes are session-level and therefore require `/tools apply`.
- System-instruction changes are also setup-level and therefore require
  `/system apply`.
- `/tools apply` currently starts a fresh session instead of resuming server
  conversation state.
- Local tools are intentionally narrow: `read-file` only reads UTF-8 text,
  `list-files` stays inside the current workspace root, and `run-command`
  executes argv-only commands without a shell.
- Screen-share startup uses the saved numeric target id. If monitor/window
  ordering changes between runs, the saved target may no longer point at the
  same surface.
