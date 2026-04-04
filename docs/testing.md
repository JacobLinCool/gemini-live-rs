# Testing Strategy

> Test coverage tracking. Planned tests are tracked in [`roadmap.md`](roadmap.md) items **T-1** through **T-6**.

---

## Implemented Tests

| Level | Test item | Count |
|---|---|---|
| **Unit** | Codec: serialize / deserialize round-trip | 20 |
| **Unit** | AudioEncoder: f32/i16 → base64 correctness | 5 |
| **Unit** | ReconnectPolicy: backoff calculation | 1 |
| **Unit** | Session: status encoding, resume handle tracking | 3 |
| **Unit** | Transport: URL construction, default config | 4 |
| **Doc** | `lib.rs` usage example, `AudioEncoder` example | 3 |

**Total: 36 unit + 3 doc tests**

## Running Tests

```bash
# Unit tests (no network)
cargo test

# Integration tests (requires API key)
GEMINI_API_KEY=xxx cargo test -- --ignored
```
