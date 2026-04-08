# Testing Strategy

> Test coverage tracking. Planned tests are tracked in [`roadmap.md`](roadmap.md) items **T-1** through **T-8**.

---

## Implemented Tests

| Level | Test item | Count |
|---|---|---|
| **Unit** | Codec: serialize / deserialize round-trip | 22 |
| **Unit** | AudioEncoder: f32/i16 → base64 correctness | 5 |
| **Unit** | ReconnectPolicy: backoff calculation | 1 |
| **Unit** | Session: status encoding, resume handle tracking, resumed-handshake setup shaping | 4 |
| **Unit** | Transport: URL construction, default config | 4 |
| **Unit** | IO crate: desktop audio resample helpers | 2 |
| **Unit** | Runtime crate: staged setup patching + resumed/fresh apply semantics + managed runtime event/tool orchestration + hot/dormant session-manager coverage | 13 |
| **Unit** | CLI: startup config + top-level CLI parsing + slash parser/completion + render status + app reducer + outbound send flow + tool catalog | 30 |
| **Unit** | Discord crate: config parsing + target-channel planning + owner/text routing policy + runtime bootstrap + service helper behavior | 31 |
| **Doc** | `lib.rs` usage example, `AudioEncoder` example | 4 |
| **Bench** | Criterion hot-path suite (`cargo bench -p gemini-live`) | 1 |

**Total: 46 library unit + 2 IO unit + 13 runtime unit + 30 CLI unit + 31 Discord unit + 4 doc tests + 1 benchmark suite**

## Running Tests

```bash
# Unit tests (no network)
cargo test

# Integration tests (requires API key)
GEMINI_API_KEY=xxx cargo test -- --ignored

# Benchmarks (performance baselines are manual today)
cargo bench -p gemini-live
```
