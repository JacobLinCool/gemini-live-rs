# gemini-live-harness

Durable file-backed harness state, shared host-tool execution contracts, and
host-side orchestration for Gemini Live applications.

This crate sits above `gemini-live-runtime` and below concrete hosts. It keeps
tasks, notifications, and long-lived memory on disk so state survives process
restarts and remains inspectable by other local agents. It also owns the
host-fed `ToolProvider` / `ToolExecutor` surface used by hosts and reusable
tool families, plus the harness-side execution wrapper that can keep blocking
tools inline within a latency budget and spill eligible calls into durable
background tasks. The preferred host boundaries are:

- `HarnessController` for durable tools + passive notification delivery
- `HarnessRuntimeBridge` for runtime tool-call orchestration and completion forwarding
