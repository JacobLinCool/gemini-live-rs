# Testing Strategy

> Test coverage tracking. Planned tests are tracked in [`roadmap.md`](roadmap.md) items **T-1** through **T-8**.

---

## Implemented Tests

Current checked-in coverage is organized by crate and behavior surface rather
than by a brittle per-topic count snapshot:

- `gemini-live`
  codec round-trips, event decomposition, audio encoding, session status and
  resumed-handshake shaping, transport request construction, and wire-level
  hot-path benchmarks.
- `gemini-live-runtime`
  staged setup patching, resumed vs fresh apply semantics, managed runtime
  forwarding, generation filtering, process-local memory, hot/dormant
  session-manager behavior, idle-timeout boundary regression, and managed
  runtime forwarding benchmarks.
- `gemini-live-harness`
  durable task and notification storage, passive notification delivery,
  profile-scoped storage helpers, tool-execution budgeting, controller
  orchestration, runtime-bridge forwarding, signal-driven notification wake
  regression, and passive-notification durability benchmarks.
- `gemini-live-cli`
  startup/profile resolution, CLI argument parsing, slash-command parsing and
  completion, reducer behavior, render status, outbound send ordering, and tool
  catalog/runtime composition.
- `gemini-live-discord`
  config parsing, routing policy, target-channel planning, runtime bootstrap,
  service helper behavior, and current text/voice projection semantics.
- `gemini-live-io`
  desktop audio resample helpers.
- Doc tests
  `gemini-live` crate docs, including the `lib.rs` usage example and
  `AudioEncoder` examples.

The source of truth for exact test counts is the test modules themselves plus
`cargo test --workspace --all-targets`; avoid keeping an exact count table here
because it drifts whenever tests are added or reorganized.

## Running Tests

```bash
# Checked-in tests and doc tests
cargo test --workspace --all-targets

# Wire-level send/recv hot path
cargo bench -p gemini-live

# Runtime event-forwarding baselines
cargo bench -p gemini-live-runtime --bench managed_runtime

# Harness notification-delivery baselines
cargo bench -p gemini-live-harness --bench passive_notification
```

There are no checked-in real-API integration tests yet. Those gaps are tracked
in [`roadmap.md`](roadmap.md) items **T-1** through **T-6**.

Benchmark baselines are still reviewed manually today, but the checked-in bench
targets now cover the wire client, managed runtime forwarding, and durable
passive-notification delivery instead of only the lowest protocol layer.
