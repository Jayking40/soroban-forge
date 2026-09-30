# PR: Safe owner rotation with pending-queue revalidation

Closes #247

## Description

Adds `add_owner`, `remove_owner`, and `set_threshold` entrypoints to `crates/multi-sig-wallet`. Enables safe signer set rotation and threshold modification while strictly preserving threshold invariants (`1 <= threshold <= owners.len()`) and preventing pending-queue corruption. 

When an owner is removed, their confirmations across all pending transactions are systematically revoked, transaction confirmation counts are revalidated, and any pending transaction dropping below the threshold is immediately prevented from executing until sufficient valid signatures are collected.

---

## Governance & Design Decisions

### 1. Governance Model
- **Self-referential wallet execution (`threshold-of-owners` approval)**:
  Rotation actions (`add_owner`, `remove_owner`, `set_threshold`) are executed either via threshold-approved multi-sig wallet proposals (self-auth via `env.current_contract_address().require_auth()`) or an explicit governor role captured safely at initialization.
- **Trust Trade-off**: Ensures existing multi-sig signers retain custody over wallet membership without introducing an external single point of failure or centralized key.

### 2. Pending Transaction Revalidation Semantics
- **Owner Removal**:
  - The removed owner's `DataKey::Confirmed(tx_id, removed_owner)` entries are removed from pending txs.
  - Confirmation tallies for pending transactions are decremented by 1 if the removed owner had confirmed them.
  - Transactions dropping below `threshold` lose executable status until re-confirmed by another current owner.
- **Owner Addition**:
  - Adds the new owner to the `Owners` instance set.
  - New owner is immediately authorized to confirm pre-existing and future pending transactions.
- **Threshold Adjustment**:
  - Modifies instance storage `Threshold`.
  - Re-evaluates executability across pending transactions dynamically against the new threshold.

---

## Rotation State Table

| Action | Pre-condition | Instance Storage Mutation | Pending Confirmation Effect | Executability Outcome |
|---|---|---|---|---|
| `add_owner(new_owner)` | `new_owner ∉ owners` | `owners.push_back(new_owner)`, `count += 1` | None | Existing pending transactions unchanged; new owner can confirm |
| `remove_owner(owner)` | `owner ∈ owners`, `owners.len() - 1 >= threshold` | `owners.remove(owner)`, `count -= 1` | If `owner` confirmed `tx`: confirmation revoked, `tx.confirmations -= 1` | `tx.confirmations < threshold` becomes non-executable; `tx.confirmations >= threshold` remains executable |
| `set_threshold(new_threshold)` | `1 <= new_threshold <= owners.len()` | `threshold = new_threshold` | None | All pending txs evaluated against `new_threshold` on `execute` |
| `remove_owner` (causes `threshold > remaining`) | `owners.len() - 1 < threshold` | **Rejected** with `ForgeError::InvalidInput` | N/A | Invariant `1 <= threshold <= count` strictly preserved |

---

## Property-Based & Integration Tests

### Integration Test Scenarios
1. `add_owner_success`: Inserts owner, preserves existing confirmation counts and threshold.
2. `remove_owner_with_empty_queue`: Safe removal when no pending transactions exist.
3. `remove_owner_revokes_confirmations`: Owner removal decrements confirmation count and prevents execution if dropped below threshold.
4. `remove_owner_threshold_met_survives`: If remaining confirmations still meet threshold, transaction remains executable.
5. `set_threshold_enforces_bounds`: Rejects `0` or values strictly greater than `owners.len()`.
6. `double_add_or_double_remove_rejected`: Rejects duplicate additions and nonexistent removals with `ForgeError::InvalidInput` / `NotFound`.
7. `unauthorized_rotation_rejected`: Enforces authorization check on all rotation entrypoints.

### Property Test Strategy
- Arbitrary sequences of `propose`, `confirm`, `add_owner`, `remove_owner`, `set_threshold`, and `execute` operations:
  - **Invariant 1**: At all times, `1 <= threshold <= owners.len()`.
  - **Invariant 2**: No transaction can execute unless `confirmations(valid_active_owners) >= current_threshold`.
  - **Invariant 3**: No removed owner confirmation is counted toward threshold.

---

## Verification

- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --workspace --all-targets --locked -- -D warnings`
- [x] `cargo test -p soroban-forge-multi-sig-wallet --all-targets --locked`
- [x] `cargo test --workspace --all-targets --locked`
- [x] WASM size within budget.

---

## Type of Change

- [x] New feature (non-breaking addition to multi-sig wallet trait)
- [x] Property test & integration test suite addition
- [x] Documentation updates (`docs/contracts/multi-sig-wallet.md`, `docs/FEATURE-STATUS.md`)

---

## Checklist

- [x] Follows the repository state-machine discipline (validate → auth → mutate → revalidate → persist).
- [x] Preserves persistent-storage discipline for transaction records and instance-storage discipline for owners/threshold.
- [x] Emits events (`owner_added`, `owner_removed`, `threshold_changed`) matching record views.
- [x] All CI checks passing across the workspace.
