# Testing Strategy

> Test coverage tracking. Planned tests are tracked in [`roadmap.md`](roadmap.md) items **T-1** through **T-8**.

---

## Implemented Tests

Current checked-in coverage is organized by crate and behavior surface rather
than by a brittle per-topic count snapshot:

- `gemini-live`
  codec round-trips, event decomposition, audio encoding, session status and
  resumed-handshake shaping, transport request construction, and hot-path
  benchmarks.
- `gemini-live-runtime`
  staged setup patching, resumed vs fresh apply semantics, managed runtime
  forwarding, generation filtering, process-local memory, and hot/dormant
  session-manager behavior.
- `gemini-live-harness`
  durable task and notification storage, passive notification delivery,
  profile-scoped storage helpers, tool-execution budgeting, controller
  orchestration, and runtime-bridge forwarding.
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

# Benchmarks (performance baselines are still manual today)
cargo bench -p gemini-live
```

There are no checked-in real-API integration tests yet. Those gaps are tracked
in [`roadmap.md`](roadmap.md) items **T-1** through **T-6**.
