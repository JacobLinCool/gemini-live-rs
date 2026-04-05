# gemini-live-cli

Interactive TUI client for the Gemini Multimodal Live API.  Serves as both
a practical tool and a living usage example of the `gemini-live` library.

## Quick Start

```bash
GEMINI_API_KEY=your-key cargo run -p gemini-live-cli
```

Override the model:

```bash
GEMINI_MODEL=models/gemini-2.5-flash-native-audio-latest cargo run -p gemini-live-cli
```

## UI Layout

```
┌─ models/gemini-3.1-flash-live-preview ──────────────────┐
│   connected — @file for media, /mic /speak for audio    │
│ [you] hello                                             │
│ [you] (transcription) 你好                               │  ← mic input transcript
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

### Slash Commands

| Command | Action |
|---------|--------|
| `/mic` | Toggle microphone input. Captures from the default input device and streams to the API at the device's native sample rate. |
| `/speak` | Toggle speaker output. Plays model audio (24 kHz PCM) through the default output device, resampled to the device's native rate. When both mic and speak are on, WebRTC AEC (Acoustic Echo Cancellation) automatically removes speaker echo from the mic signal. |
| `/share-screen list` | List available capture targets (monitors and windows) with IDs. |
| `/share-screen <id> [interval]` | Start sharing a monitor or window. `interval` is seconds between frames (default: 1). Example: `/share-screen 0 5` shares Display 1 every 5 seconds. |
| `/share-screen` | Stop screen sharing (when active). |

### Keyboard

| Key | Action |
|-----|--------|
| `Enter` | Send input / execute command |
| `Backspace` | Delete last character |
| `Esc` / `Ctrl-C` / `Ctrl-D` | Quit |

## Feature Flags

Each slash command group is a separate Cargo feature, all enabled by default:

| Feature | Dependencies | Enables |
|---------|-------------|---------|
| `mic` | `cpal`, `webrtc-audio-processing` | `/mic` command with AEC |
| `speak` | `cpal`, `webrtc-audio-processing` | `/speak` command with AEC |
| `share-screen` | `xcap`, `image` | `/share-screen` command |

Build without audio/screen support for a minimal binary:

```bash
cargo build -p gemini-live-cli --no-default-features
```

## Architecture

```
main.rs      TUI event loop (crossterm + ratatui), command dispatch
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
