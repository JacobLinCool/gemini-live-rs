# Development Guidelines

## Documentation Philosophy: Code-First, Progressive Disclosure

Documentation lives **in the source code**, not beside it.

- If a fact can be attached to a specific module, type, field, enum variant,
  function, or CLI command, that is its **natural home** and it should be
  documented there first.
- **Module-level doc comments** give the architectural overview and explain
  *why* this module exists.
- **Struct / enum / function doc comments** explain semantics, constraints,
  and wire-format mapping.
- **Standalone docs (`docs/`)** are reserved for content that has **no
  natural home in code**: design rationale (ADRs), cross-cutting comparison
  tables, official reference links, and roadmap.

As implementation progresses, actively **migrate** content from standalone
docs into source code comments and **trim** the standalone docs. The goal is
to make the code the single source of truth for anything code-representable.
Standalone docs should shrink over time, not grow.

Use the code's own structure for progressive disclosure: a reader entering at
the crate root (`lib.rs`) should get a high-level map; drilling into a module
should reveal layer-specific context; drilling into a type should reveal
field-level protocol detail. Don't front-load everything in one file.

### Natural Home Test

Before adding or updating documentation, ask:

1. Can this statement be attached to a specific code artifact without losing context?
2. Would a maintainer editing that code reasonably need to see this statement
   at the point of change?
3. Can this statement be kept correct more easily next to the implementation
   than in a separate document?

If the answer is "yes", put it in code. Use standalone docs only when the
content is genuinely cross-cutting, archival, or external-reference-heavy.

### Drift Policy

- **Code and code-adjacent docs win.** If a standalone doc disagrees with the
  implementation or adjacent doc comments, treat the standalone doc as stale
  and fix it immediately.
- **Update adjacent docs in the same patch.** When changing behavior, update
  the relevant module/type/function/field doc comments as part of the same
  code change, not as a later cleanup.
- **Standalone docs should point inward.** Prefer summaries, link tables, and
  cross-references into source files rather than duplicating behavior that is
  already documented in code.
- **Do not maintain two canonical explanations** for the same behavior. If a
  standalone doc must mention it, keep the standalone version brief and make
  the source comment the canonical one.

## Aligning with Official API References

When building a client for an external API:

1. **Wire format is the contract.** Every struct, enum variant, and serde
   attribute must match the official API documentation exactly. When in doubt,
   the official reference wins over convenience.
2. **Maintain a reference link table** in a standalone doc so contributors
   (human or AI) can quickly verify against upstream. Include direct URLs to
   the relevant API reference pages.
3. **Track coverage gaps.** Keep a visible record (e.g. a checklist or table
   in docs) of which official features are implemented, partially
   implemented, or missing. This is the primary input for prioritisation.
4. **Pin to a specific API version / endpoint** in the code (e.g. `v1beta`,
   `v1alpha`). When the API evolves, update the version explicitly rather than
   silently drifting.
5. **Audit regularly.** When official docs change (new fields, deprecated
   features, behavioural changes), the client must follow. Treat undocumented
   divergence as a bug.
6. **Put upstream semantics next to the wire types.** Field meanings,
   model-specific constraints, and version caveats belong on the Rust types
   that encode them whenever they have a natural home there.

## Evolving the Client

The crate must stay useful as the upstream API evolves. Design for this:

- **Prefer `Option<T>` + `skip_serializing_if` for new fields** — adding a
  new optional field is always backward-compatible.
- **Use `#[serde(deny_unknown_fields)]` sparingly** — server responses may
  gain new fields at any time; leniency during deserialization prevents
  breakage.
- **Provide an escape hatch** (e.g. `send_raw`) so users can work around
  missing types without waiting for a library update.
- **Keep a `serde_json::Value` fallback** only for fields whose schema is
  genuinely unstable or unspecified (e.g. `groundingMetadata`). For
  everything else, use strongly-typed structs.

## Coding Conventions (Rust)

- **Serde attributes mirror the wire format**:
  `#[serde(rename_all = "camelCase")]` for JSON objects,
  `#[serde(rename_all = "SCREAMING_SNAKE_CASE")]` for enums that map to
  uppercase wire values. Each variant or field gets a rename only when the
  default rule doesn't match.
- **Layered errors**: each architectural layer (transport, codec, session)
  has its own error enum via `thiserror`. Upper layers wrap lower-layer
  errors via `#[from]`. Callers can match at any granularity.
- **Test codec round-trips first.** Before any integration test, verify that
  every message type serialises to the expected JSON structure and
  deserialises back. These tests are cheap, fast, and catch 90% of protocol
  bugs.
- **Derive conservatively**: `Debug` and `Clone` on all public types.
  `Default` on config structs where `..Default::default()` patterns are
  expected. `PartialEq` where useful for testing. Avoid `Copy` on types that
  may grow.

## Working with This Repository

Project-specific docs are split by change-trigger:

- [`docs/protocol.md`](docs/protocol.md)
  Content: upstream reference index, official links, model lifecycle table,
  cross-cutting protocol comparisons, and facts that do not belong on one
  specific type.
  Update when: upstream API changes.
- [`docs/design.md`](docs/design.md)
  Content: architecture diagram, performance goals, and ADRs.
  Update when: we refactor our architecture.
- [`docs/runtime-sequence.md`](docs/runtime-sequence.md)
  Content: cross-layer sequence diagrams for host/runtime/harness/session
  interactions, including background tools and passive notifications.
  Update when: we change cross-layer control flow, ordering, or failure
  semantics.
- [`docs/roadmap.md`](docs/roadmap.md)
  Content: planned work, performance gaps, and tech debt; the single source of
  truth for "identified but not done".
  Update when: we identify or complete work items.
- [`docs/cli.md`](docs/cli.md)
  Content: CLI entry-point guide, usage, command index, and high-level
  architecture. Command behavior details should live next to the CLI code when
  possible.
  Update when: we change CLI features.
- [`docs/testing.md`](docs/testing.md)
  Content: implemented test inventory and run instructions.
  Update when: we add or change tests.

- Run `cargo test` to verify all unit and doc tests. Integration tests that
  hit the real API require a `GEMINI_API_KEY` environment variable.
- The CLI crate (`gemini-live-cli`) serves as a living usage example — keep it
  in sync with library API changes.
- When a standalone doc grows because of implementation detail, stop and ask
  whether that detail should be migrated into source comments instead.
