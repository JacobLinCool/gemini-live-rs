# Testing Strategy

> Test coverage tracking. Planned tests are tracked in [`roadmap.md`](roadmap.md) items **T-1** through **T-7**.

---

## Implemented Tests

| Level | Test item | Count |
|---|---|---|
| **Unit** | Codec: serialize / deserialize round-trip | 20 |
| **Unit** | AudioEncoder: f32/i16 → base64 correctness | 5 |
| **Unit** | ReconnectPolicy: backoff calculation | 1 |
| **Unit** | Session: status encoding, resume handle tracking | 3 |
| **Unit** | Transport: URL construction, default config | 4 |
| **Unit** | CLI | 0 |
| **Doc** | `lib.rs` usage example, `AudioEncoder` example | 3 |
| **Bench** | Criterion hot-path suite (`cargo bench -p gemini-live`) | 1 |

**Total: 36 library unit + 0 CLI unit + 3 doc tests + 1 benchmark suite**

## Running Tests

```bash
# Unit tests (no network)
cargo test

# Integration tests (requires API key)
GEMINI_API_KEY=xxx cargo test -- --ignored

# Benchmarks (performance baselines are manual today)
cargo bench -p gemini-live
```
