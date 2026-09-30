# Testing Strategy

## Unit Tests

Each contract should have inline unit tests for core business logic.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    // ... tests
}
```

## Integration Tests

Located in `tests/integration/`. Spin up a soroban environment and interact with deployed WASM.

## Fuzzing

Consider using `cargo-fuzz` for parsing inputs and complex state machines.

## Chaos Testing

Persistent-storage contracts in this workspace must keep their records alive
between multi-step interactions. A silent failure mode is an entry whose TTL
expires between two legitimate calls: the next read returns `NotFound` (or
worse, a recreated default), corrupting state without surfacing an error.

Use the shared TTL chaos harness in
[`soroban-forge-test-utils::ttl`](crates/test-utils/src/ttl.rs) to exercise
contracts across arbitrary ledger gaps:

- [`advance_ledger`](crates/test-utils/src/ttl.rs) moves the test [`Env`]
  ledger sequence forward deterministically.
- [`entry_is_live`](crates/test-utils/src/ttl.rs) and
  [`entry_ttl`](crates/test-utils/src/ttl.rs) introspect a contract's
  persistent storage TTL from the test harness.
- [`chaos_drive`](crates/test-utils/src/ttl.rs) runs a seeded schedule of
  randomized operations and ledger advances against any contract that
  implements the [`ChaosTarget`](crates/test-utils/src/ttl.rs) trait. The same
  seed reproduces the same trace, so CI failures are replayable.

Demo suites for the two richest persistent-storage contracts are in:

- `crates/escrow/src/ttl_chaos.rs` — create → deposit → release/refund over
  arbitrary ledger gaps.
- `crates/multi-sig-wallet/src/ttl_chaos.rs` — submit → confirm → execute
  over arbitrary ledger gaps.

Each demo asserts the **no `NotFound` during legitimate flows** property and
prints a sample trace for inspection. When a storage refactor breaks TTL
discipline, the harness reports the seed and the exact step so the violation
can be replayed locally.

Guidelines for adding a new contract to the harness:

1. Define a small, closed enum of operations (`Create`, `Deposit`, ...).
2. Implement `ChaosTarget` with `enabled_ops` that only exposes operations
   valid in the current state.
3. In `apply_op`, classify contract errors: expected state-machine failures
   are `ExpectedFailure`; `ForgeError::NotFound` on a record that should exist
   is a `Violation`.
4. In `check_invariants`, assert every persistent record touched by the
   legitimate flow remains live via `entry_is_live`.
5. Pin the seed and assert `assert_no_violations(&trace)`.

## Coverage

Target >= 90% line coverage for stable contracts.

## CI Gates

All PRs must pass:
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --all -- --check`
