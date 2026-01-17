# Recommendation B: Targeted Fixes (REJECTED)

**Status:** REJECTED
**Date:** 2026-01-17
**Rejected Because:** "High effort" framing is obsolete with agentic coding. The architectural debt remains.

## Original Proposal

Fix the lifecycle bug without architectural changes:

1. Check `AdapterSettings` before calling `adapter.start()` in `main.rs`
2. Add adapter status API for UI
3. Document distributed pattern as intentional

## Why It Was Proposed

The superego review prioritized:
- Less code churn at RC stage
- Ship now, iterate later
- "Idiomatic Rust" argument for distributed state

## Why It Was Rejected

1. **"High effort" is irrelevant** - Agentic coding makes multi-file refactors trivial
2. **"Idiomatic Rust" is weak** - Rust has excellent support for event-driven patterns (tokio channels, actor frameworks)
3. **The lifecycle bug IS architectural** - Scattered settings checks invite future bugs; there's no coordinator to make decisions cleanly
4. **Technical debt accumulates** - Every new adapter would need to remember the pattern

## The Scattered Pattern Problem

```rust
// This invites bugs - every adapter needs this check
if settings.adapters.openhome {
    openhome.start().await;
}
if settings.adapters.lms {
    lms.start().await;
}
if settings.adapters.roon {
    roon.start().await;
}
// ... what happens when someone adds a new adapter and forgets?
```

## Conclusion

Option B would fix the immediate UX issues but leave the architecture fragile. With agentic coding removing the effort barrier, there's no reason to accept this technical debt.

See [ARCHITECTURE-RECOMMENDATION-A.md](./ARCHITECTURE-RECOMMENDATION-A.md) for the approved approach.
