# Feature Status Matrix

Per-entrypoint status across all six contracts. **Implemented** means:
implemented, tested, and covered by workspace CI. The escrow contract is
the flagship: it moves real SEP-41 tokens; marketplace royalties settles
its splits the same way via `settle_sale` and, for batches, `settle_sales`,
and `distribute` now pays the royalty share directly; vesting settles its
claims via
`claim`. Subscription charges now settle directly in pull mode and support
opt-in prepaid custody, period debits, and refunds; DAO governance dispatches approved opaque actions
on-chain and settles a SEP-41 proposal bond — pulled at `propose`, refunded
to the proposer or forfeited to the treasury at a terminal transition.
Multi-sig wallet transactions live in persistent storage with a
permissionless TTL keeper, alongside its threshold-and-confirmation state
machine.
its splits the same way via `settle_sale`; vesting settles its claims via
`claim`; subscriptions bill each due period with a real subscriber →
provider transfer in pull mode or prepaid contract-balance debit, with retry
and refund behavior; DAO governance now
dispatches approved opaque actions on-chain and settles a SEP-41 proposal
bond — pulled at `propose`, refunded to the proposer or forfeited to the
treasury at a terminal transition.

**Last verified against:** the SDK 27 migration (workspace v0.2.0).

---

## Escrow (`crates/escrow`) — **flagship**

| Entrypoint | Status | Notes |
| :--- | :---: | :--- |
| `create_escrow` | ✅ Implemented | Validates amount/timeout, buyer+seller auth, takes the SEP-41 token address |
| `deposit` | ✅ Implemented | **Real token transfer** buyer → contract, before any state write |
| `release` | ✅ Implemented | Seller-authorized; **real token transfer** contract → seller |
| `refund` | ✅ Implemented | Seller pre-deadline / buyer post-deadline; **real token transfer** |
| `refund_expired` | ✅ Implemented | Permissionless strictly after timeout; `Funded` only, disputed escrows frozen |
| `release` | ✅ Implemented | Seller-authorized; **real token transfer** contract → seller (full remaining balance) |
| `release_partial` | ✅ Implemented | Seller-authorized; **real token transfer** of a partial amount contract → seller; `released` accounting tracked; final partial transitions to `Completed`; `refund`/`resolve` operate on remaining balance |
| `refund` | ✅ Implemented | Seller pre-deadline / buyer post-deadline; **real token transfer** of remaining balance |
| `dispute` | ✅ Implemented | Claimant (buyer or seller) authorized, `Funded` only — see [design notes](KNOWN-LIMITATIONS.md#design-notes-not-limitations-but-worth-knowing) |
| `resolve` | ✅ Implemented | Arbiter-only, final; pays **remaining balance** either direction via **real token transfer** |
| `cancel` | ✅ Implemented | Buyer, `Pending` only; removes id from each distinct participant index after validation/auth |
| `get_status` / `get_escrow` / `escrows_for_participant` | ✅ Implemented | Read-only views; participant index excludes cancelled ids but retains other terminal records; live offset pagination, restart at cursor 0 after cancellation |
| `touch_ttl` | ✅ Implemented | Permissionless TTL keeper for the escrow's persistent entry |
| Events | ✅ Implemented | Includes distinct `RefundExpired` keeper event; escrow id as topic |
| Storage | ✅ Persistent + TTL | Per-id persistent entries; instance storage only for the id counter |
| Tests | ✅ 55 | Full lifecycle, dispute paths, expiry-refund boundary and event coverage, failure ordering, conservation property, **randomized property suite** (proptest), and **negative-auth suite** (`authz.rs`) |
| Events | ✅ Implemented | `EscrowCreated`, `Deposited`, `Released`, `PartiallyReleased`, `Refunded`, `Disputed`, `Resolved`, `Cancelled`; id as topic |
| Storage | ✅ Persistent + TTL | Per-id persistent entries; instance storage only for the id counter; **backward-compatible schema migration** via `EscrowDataV1` fallback decode (old records default `released = 0`) |
| Tests | ✅ 85 | Full lifecycle, dispute paths, cancellation index maintenance and live-pagination interleaving, failure ordering, conservation property, partial-release (valid/multi/final/zero/negative/over-remaining/non-Funded/after-completion/→refund/→dispute→resolve, storage compat, conservation), **randomized property suite** (proptest): conservation over random paths + partial-release sequences, tamper-resilient pool conservation, fund safety over arbitrary call sequences (now includes `release_partial`), multi-party create/cancel participant-index consistency; **negative-auth suite** (`authz.rs`): per-entrypoint wrong-signer rejection, `release_partial` seller-only auth + mutation test, signature/args replay rejection, `env.auths()` authorization-tree assertions |

## Vesting (`crates/vesting`)

| Entrypoint                    | Status             | Notes                                                                                                                                                                                                                                |
| ----------------------------- | ------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `create_schedule`             | ✅ Implemented     | Validates `total_amount > 0`, `duration > 0`, `cliff <= duration`                                                                                                                                                                    |
| `claim`                       | ✅ Implemented     | **Real token transfer** contract → beneficiary before the state write (transfer-before-state); zero-claim calls skip the transfer; a failed transfer surfaces as `ForgeError::TokenTransferFailed` with `claimed`/`status` unchanged |
| `claimable`                   | ✅ Implemented     | Read-only                                                                                                                                                                                                                            |
| `get_status`                  | ✅ Implemented     | Read-only; derived from ledger time + claimed amount, so it is current between claims                                                                                                                                                |
| `get_schedule`                | ✅ Implemented     | Read-only record view of the linear schedule (issue #125), mirroring `get_tranche_schedule` and the other crates' record views; `NotFound` for unknown ids and tranche ids; no auth, no state change; `claimed` reflects completed claims |
| Revocation                    | ❌ Not implemented | `Revoked` status reserved                                                                                                                                                                                                            |
| `VestingSchedule.token` field | ✅ Wired           | Read by `claim` for the SEP-41 payout                                                                                                                                                                                                |

## Multi-Sig Wallet (`crates/multi-sig-wallet`)

| Entrypoint                                         | Status         | Notes                                                                                                                                                                                                                   |
| -------------------------------------------------- | -------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `initialize`                                       | ✅ Implemented | Owner set + threshold validation                                                                                                                                                                                        |
| `submit` / `submit_call`                           | ✅ Implemented | Creates pending transaction record with optional expiry deadline in **persistent storage** (TTL-bumped on write); supports opaque payloads and typed calls                                                               |
| `confirm`                                          | ✅ Implemented | One-confirmation-per-owner enforced; tx record re-bumped in persistent storage; boundary-pinned rejection (`ForgeError::DeadlineReached`) at or after expiry                                                             |
| `execute`                                          | ✅ Implemented | Threshold check + cross-contract `try_invoke_contract` to recorded `target`; status flip **after** invocation; target revert surfaces as `ForgeError::ContractInvocationFailed` and leaves tx `Pending`; reaching threshold before expiry remains executable after deadline |
| `get_threshold` / `get_tx` / `is_live`             | ✅ Implemented | Read-only; `get_tx` reads from persistent storage with lazy expiry evaluation (`TxStatus::Expired`); `is_live` / `is_tx_live` reports whether tx is live and pending                                                   |
| `submit_withdrawal`                                | ✅ Implemented | Typed `TxKind::Withdrawal` tx; balance validated at execution (transfer first, state second); per-token rolling limit enforced at submission                                                                            |
| `deposit` / `balance` / `touch_ttl`                | ✅ Implemented | Per-token persistent balance entries; permissionless TTL keeper                                                                                                                                                         |
| `touch_tx_ttl`                                     | ✅ Implemented | Permissionless keeper: extends the persistent TTL of a transaction record without touching its state; `NotFound` for unknown ids                                                                                         |
| `set_withdrawal_limit` / `remove_withdrawal_limit` | ✅ Implemented | `TxKind::LimitChange` txs on the same threshold+confirmation machinery; no effect below threshold                                                                                                                       |
| `get_withdrawal_limit` / `get_withdrawal_window` / `get_window_usage` / `check_withdrawal` | ✅ Implemented | Read-only; missing limit/window reads as `None`/empty; simulation reports initialization, amount, limit, and custody-balance outcomes; no storage mutation |
| `add_owner` / `remove_owner` / `set_threshold`     | ✅ Implemented | Owner-set and threshold governance via typed-tx machinery: `TxKind::AddOwner` / `RemoveOwner` / `SetThreshold`; must cross threshold before `execute` applies them                                                      |
| `get_owners` / `is_owner` / `get_confirmations` / `get_rejections` / `get_tx_count` / `get_transactions` / `get_transactions_by_status` | ✅ Implemented | Read-only views; `get_transactions_by_status` includes `TxStatus::Expired`                                                                                                             |
| Storage                                            | ✅ Persistent + TTL | `DataKey::Tx` records in persistent storage (migrated from instance) with 30-day TTL maintenance; owners, threshold, and limits remain instance storage                                                                |
| Tests                                              | ✅ 155         | Full lifecycle, threshold/quorum, withdrawal + limit-change paths, per-tx expiry with lazily evaluated Expired state and boundary-pinned confirmations, keeper TTL tests, negative-auth suite (`authz.rs`), proptest suite (`props.rs`) |

## DAO Governance (`crates/dao-governance`)

| Entrypoint | Status | Notes |
|---|---|---|
| `initialize` | ✅ Implemented | One-time permissionless configuration of the SEP-41 governance token used to weight votes |
| `configure_bond` | ✅ Implemented | One-time permissionless config (token, amount, treasury); first caller wins; `propose` is rejected with `NotInitialized` while unconfigured |
| `propose` | ✅ Implemented | Stores target, action, deadline, `requires` and one optional `conflicts_with`; validates dependencies and cycles; enforces the proposer active-proposal limit of 5; bond transfer before writes |
| `vote` | ✅ Implemented | One-vote-per-voter; adds the voter's current governance-token balance to the selected tally; zero-balance votes rejected |
| `execute` | ✅ Implemented | Permissionless majority finalisation and dependency-gated dispatch; unmet requirements return `DeadlineReached`, executed conflicts return `InvalidInput`, and cancelled/defeated requirements permanently strand dependents in `Succeeded`; target failure remains retryable; terminal transitions decrement proposer active count |
| `cancel_proposal` | ✅ Implemented | Proposer-authorized revocation; bond refund; decrements proposer active count |
| `get_proposal` / `get_dependencies` / `get_proposal_count` / `get_proposals` / `has_voted` / `get_active_proposal_count` | ✅ Implemented | Read-only proposal and dependency-edge views; pagination with bounds clamping and proposer active count |
| `get_bond_config` | ✅ Implemented | Read-only; `NotInitialized` when no bond is configured |
| `touch_ttl` | ✅ Implemented | Permissionless keeper: extends the persistent TTL of a proposal; `NotFound` for unknown ids |
| Events | ✅ Implemented | `Proposed` data includes dependency edges; event names/topics otherwise stable; `VoteCast`, `Finalised`, `BondPosted`, `BondReleased` |
| Storage | ✅ Persistent + TTL | Dependencies live on persistent proposal records; no new top-level keys; count, config, custody, vote markers, and proposer active counts remain in instance storage |
| Tests | ✅ 85+ | Bond custody, weighted voting and cooldown coverage; dependency ordering/conflicts/cancellation, malformed graph validation, dependency view/events, randomized execution-order mirror property; negative-auth suite retained |
| Weighted voting | ✅ Implemented | Balance-weighted voting powered by immutable SEP-41 governance token configured at `initialize` |

## Subscription Payments (`crates/subscription-payments`)

| Entrypoint | Status | Notes |
|---|---|---|
| `subscribe` | ✅ Implemented | Validates `amount > 0` / `period > 0` before auth; subscriber-authorized; record + sequential id + both subscriber/provider indexes written atomically |
| `subscribe_on_behalf_of` | ✅ Implemented | Provider-initiated; requires a **subscriber opt-in** (`ProviderOptIn`) checked before provider auth; shares the same id counter and record shape as `subscribe` |
| `authorize_provider` / `revoke_provider` / `is_provider_authorized` | ✅ Implemented | Explicit per-relationship opt-in; subscriber-authorized; idempotent; read-only view has no auth |
| `charge` | ✅ Implemented | Provider-authorized; pull mode transfers subscriber → provider; prepaid mode transfers an exact period amount from contract custody → provider; a subscription whose due time has elapsed is observable as `PastDue` (time-derived), and a successful catch-up charge bills exactly one overdue period, restores `Active`, and advances the due timestamp one period; insufficient prepaid balance follows retry → `PastDue`, auto-cancelling after 3 failures with exact remainder refund |
| `deposit` / `withdraw_balance` | ✅ Implemented | Subscriber-authorized prepaid opt-in and top-up pulls exact funds before state writes; surplus withdrawal transfers contract → subscriber before balance update; failed transfers leave state unchanged |
| `pause` / `resume` | ✅ Implemented | Subscriber-authorized; `resume` advances the next due date by the elapsed paused duration |
| `set_quotas` | ✅ Implemented | Subscriber-authorized (prices the overage the subscriber is billed); `Active` only and rejected once the open period has usage, so metered units cannot be repriced mid-period; validates ≤ `MAX_QUOTAS` (16) unique metrics, `bucket_units > 0`, `overage_price >= 0`; an empty list returns to flat pricing |
| `record_usage` | ✅ Implemented | Provider-authorized; accumulates **raw units** for the open period only; rejects undeclared metrics, zero units, non-`Active` subscriptions, and a `u64` counter overflow; meters are dropped atomically with the charge that closes the period, so a failed transfer leaves them intact and the retry bills identically |
| `quote_period` / `get_usage` | ✅ Implemented | Read-only views with no auth; `quote_period` is the same derivation `charge`/`charge_catchup` settle, so a quote and the charge cannot disagree; `get_usage` returns a zeroed record stamped with the current window for an unused metric |
| Metered overage pricing | ✅ Implemented | `base + Σ ceil(min(max(0, units - included), cap) / bucket) * price`, rounded up per bucket, derived per period from raw units so multi-period totals cannot drift; cap enforced by clamping (never rejecting) at settlement; an unrepresentable bill → `ArithmeticOverflow` before any transfer |
| Prepaid balance | ✅ Implemented | Per-subscription optional balance (`None` is unchanged pull mode; `Some(0)` remains prepaid); one-period charge only, catch-up rejected; `Deposited`, `BalanceDebited`, `BalanceRefunded`; conservation property tested against independent lifecycle mirror |
| `cancel` | ✅ Implemented | Subscriber-authorized from `Active` / `Paused` / `PastDue`; refunds exact remaining prepaid balance before `Cancelled`; rejects already-`Cancelled` |
| `get_subscription` / `get_subscription_count` / `subscriptions_for_subscriber` / `subscriptions_for_provider` | ✅ Implemented | Read-only views; paged by `offset`/`limit` with `limit == 0` → `InvalidInput`; empty index yields an empty page, not an error |
| Plan management | ❌ Not implemented | Follow-up |
| `PastDue` lifecycle | ✅ Implemented | Time-derived lapsed state (`Active → PastDue → Active`); `PastDueEntered` event per transition; recoverable only through a successful catch-up charge (never reverts by time alone); `cancel` from `PastDue` and `pause`/`resume` interactions defined and tested; one-period-per-call invariant preserved with overflow-safe `checked_add` due math |

## Marketplace Royalties (`crates/marketplace-royalties`)

| Entrypoint | Status | Notes |
|---|---|---|
| `set_royalty` | ✅ Implemented | Basis-point caps validated |
| `distribute` | ✅ Implemented | **Real SEP-41 royalty payout**: `distribute(collection, token, payer, seller, amount)` transfers `amount * bps / 10_000` from the payer to the configured recipient and returns the seller's net; a `Disabled`/zero-bps config transfers nothing; standalone settlement for sales handled outside `settle_sale`, the seller's net is not transferred here |
| `settle_sale` | ✅ Implemented | **Real token transfers** payer → seller, then payer → royalty recipient; transfer-before-state, totals committed last |
| `settle_sales` | ✅ Implemented | **Atomic batch settlement**: up to 20 sales per call against one collection + one payer authorization; every validation (cap, amounts, aggregate split math) before the first transfer, per-sale seller-then-recipient order, aggregate summary committed exactly once; any failure rolls the whole batch back |
| `get_royalty` | ✅ Implemented | Read-only |
| `get_settlement_summary` | ✅ Implemented | Read-only; cumulative sales, volume, and royalties per collection |
| `quote_sale` | ✅ Implemented | Read-only per-sale quote (issue #126): the exact split a settlement would apply (`gross`, effective `royalty_bps`, `royalty_amount`, `seller_net`) via the same `effective_bps` + `split` resolution the settlement entrypoints run; settle-parity including floor rounding, `royalty_amount + seller_net == gross`; disabled collections quote at 0 bps like `settle_sale` settles in full; `NotFound` when unregistered, `InvalidInput` for `amount <= 0`; no auth, no storage mutation, no events |
| `touch_ttl` | ✅ Implemented | Permissionless keeper: extends the persistent TTL of a collection's royalty + summary records; `NotFound` when unregistered |
| Storage | ✅ Persistent + TTL | `Royalty` and `SettlementSummary` records in persistent storage with 30-day TTL maintenance |
| Multi-recipient splits | ❌ Not implemented | Follow-up |

---

## Cross-cutting

| Concern | Status | Notes |
|---|---|---|
| Checked arithmetic | ✅ Workspace-wide | Overflow-safe; vesting guards documented |
| `require_auth` on every state change | ✅ Workspace-wide | Escrow, vesting, DAO governance, and marketplace royalties proven against wrong signers via their negative-auth suites (`authz.rs`) + authorization-tree assertions; other two: call-graph level only (see [Known Limitations §4](KNOWN-LIMITATIONS.md)) |
| Events | ✅ Escrow + Multi-Sig + DAO + Marketplace | Full lifecycle events on escrow, multi-sig wallet, DAO governance, and marketplace royalties |
| Persistent storage + TTL | ✅ Escrow + Royalties + Multi-Sig + DAO | Per-record persistent entries with `touch_ttl`/`touch_tx_ttl` keepers on escrow, marketplace royalties (royalty + summary), multi-sig (transactions), and DAO (proposals); vesting and subscriptions remain instance-only. TTL policy constants + `bump_entry` helper consolidated in shared-utils (issue #127) and consumed by all four |
| SEP-41 token settlement | ✅ Escrow + royalties + multi-sig + vesting + DAO + subscriptions | Transfers use transfer-before-state ordering; subscription pull-mode charges pay subscriber → provider, while opt-in prepaid subscriptions custody deposits and pay exact debits/refunds |
| Testnet deployment | ✅ Escrow deployed | Contract ID, WASM sha256, and receipt rounds in the README "Proof at a glance" table; the other five are not deployed |
| Mainnet deployment | ⚠️ Partial | Smoke SAC live (`CBBCLWWU…DN4CW`, Horizon-confirmed); escrow WASM upload measured at **17.57 XLM rent** via simulation and deferred pending funding — see [Known Limitations §6](KNOWN-LIMITATIONS.md) |
| TypeScript SDK | ✅ Generated | `@soroban-forge/escrow-client` generated from the deployed escrow ABI (no own test suite yet) |
| Provenance + verification | ✅ CLI | `soroban-forge verify` checks a WASM artifact / expected hash against a deterministic rebuild using `provenance-manifest.json` | 
| `require_auth` on every state change | ✅ Workspace-wide | Escrow, vesting, and DAO governance proven against wrong signers via their negative-auth suites (`authz.rs`) + authorization-tree assertions; other three: call-graph level only (see [Known Limitations §4](KNOWN-LIMITATIONS.md)) |
| Events | ⚠️ Escrow + Multi-Sig + DAO | Full lifecycle events on escrow, multi-sig wallet, and DAO governance |
| Persistent storage + TTL | ⚠️ Escrow only | Per-id persistent entries + `touch_ttl` keeper; others instance-only |
| SEP-41 token settlement | ⚠️ Escrow + royalties + multi-sig + vesting + DAO + subscriptions | Real transfers with transfer-before-state ordering on escrow, vesting, marketplace, DAO, and subscriptions; prepaid subscriptions add contract custody, exact balance debits, and cancellation refunds |
| Testnet deployment | ✅ Escrow deployed | Contract ID, WASM sha256, and receipt rounds in the README "Proof at a glance" table; the other five are not deployed |
| Mainnet deployment | ⚠️ Partial | Smoke SAC live (`CBBCLWWU…DN4CW`, Horizon-confirmed); escrow WASM upload measured at **17.57 XLM rent** via simulation and deferred pending funding — see [Known Limitations §6](KNOWN-LIMITATIONS.md) |
| TypeScript SDK | ✅ Generated | `@soroban-forge/escrow-client` generated from the deployed escrow ABI (no own test suite yet) |
| CLI | ✅ Implemented | Developer CLI with `build`, `test`, `lint`, `deploy`, `invoke`, and `events` commands |
| ForgeError registry | ✅ Generated | Machine-readable error registry from Rust source; generates `errors.json` and TypeScript module |
| TypeScript SDK | ✅ Generated | `@soroban-forge/escrow-client` from the deployed escrow ABI + five generated clients (`vesting`, `multi-sig-wallet`, `subscription-payments`, `marketplace-royalties`, `dao-governance`) via `scripts/generate-clients.sh`; all six build with `npm run build` (no own test suites yet) |
| Next.js demo | ✅ Example | `packages/nextjs-example` — testnet escrow demo UI (Freighter connect, friendbot funding, create/deposit/release/refund/dispute/resolve, read, participant list) |
| CI (fmt/clippy/test/audit/WASM size/provenance) | ✅ Enforced | `--locked`, `-D warnings`, stable toolchain, `wasm32v1-none`, size budget, **provenance manifest job** (SHA-256 of all six WASM artifacts from a clean rebuild) |
| External audit | ❌ Not performed | Planned as a grant-funded tranche deliverable before any mainnet value custody |
| Soroban SDK version | ✅ 27.0.6 | Stable Rust; `wasm32v1-none` target |
