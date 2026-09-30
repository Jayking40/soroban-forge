# Marketplace Royalties Contract

NFT / digital asset sales with configurable royalty distribution across
secondary sales — distributed by `distribute` (real SEP-41 royalty payout)
and settled atomically in real SEP-41 tokens by `settle_sale` (one sale) or
`settle_sales` (a capped, all-or-nothing batch).

## Interface

```rust
fn set_royalty(collection, recipient, bps, accrual) -> Result<(), ForgeError>
fn distribute(collection, token, payer, seller, amount) -> Result<i128, ForgeError>
fn settle_sale(collection, token, payer, seller, amount) -> Result<Settlement, ForgeError>
fn settle_sales(collection, token, payer, sales: Vec<(seller, amount)>) -> Result<Vec<Settlement>, ForgeError>
fn distribute_accrued(collection, token) -> Result<i128, ForgeError>
fn get_royalty(collection) -> Result<Royalty, ForgeError>
fn get_settlement_summary(collection) -> Result<SettlementSummary, ForgeError>
fn get_accrued(collection, token) -> Result<i128, ForgeError>
fn get_recipient_accrued(collection, token, recipient) -> Result<i128, ForgeError>
fn quote_sale(collection, amount) -> Result<SaleQuote, ForgeError>
fn touch_ttl(collection) -> Result<(), ForgeError>
```

## Concepts

- **Creators** receive a split of every sale. In payout mode the split is paid
  to the configured `recipient` on settlement; in **accrual mode** the split
  is held in contract custody on a per-`(collection, token)` ledger and only
  paid out when `distribute_accrued` is invoked.
- **Sellers** receive the net of every sale.
- **Payers** fund both transfers from their `token` balance in a single
  authorized call. In accrual mode the payer still funds the full sale amount,
  but the royalty share is pulled into contract custody rather than to the
  recipient.
- **Collections** opt into accrual at `set_royalty(collection, recipient, bps,
  accrual=true)`. The `accrual` flag is immutable once the collection has any
  settlement or accrual history; flipping it afterwards returns
  `ForgeError::InvalidState`.

## Settlement

`settle_sale` moves one sale's proceeds with escrow's transfer-before-state
ordering:

1. Load the collection's configuration (`NotFound` if unregistered), validate
   `amount > 0` (`InvalidInput`), and require the collection's and the payer's
   authorization.
2. Compute `royalty_share = amount * bps / 10_000` (floored) and
   `seller_net = amount - royalty_share` with checked arithmetic
   (`ArithmeticOverflow`), and stage the updated settlement totals. The two
   parts always sum exactly to `amount` — rounding dust stays with the
   seller.
3. In payout mode, transfer `seller_net` from `payer` to `seller`, then
   `royalty_share` from `payer` to the configured `recipient` — the recipient
   is paid last. In accrual mode, transfer `seller_net` from `payer` to
   `seller`, then transfer `royalty_share` from `payer` into contract custody
   on the `(collection, token)` ledger.
4. Only after the transfers succeed, commit the collection's cumulative
   settlement totals. In payout mode `royalties_paid` includes the
   `royalty_share`; in accrual mode `royalties_paid` is only updated when
   `distribute_accrued` later sweeps the ledger.

A `Disabled` or zero-bps configuration skips the recipient/custody transfer
and settles the full amount to the seller. Token failures (insufficient
balance, missing trustline, undeployed token) surface as
`ForgeError::TokenTransferFailed`, and any returned error rolls back the
whole invocation — a failed settlement can never leave the recipient
partially paid and never commits totals.

`distribute` is a standalone royalty settlement: in payout mode it transfers
only the royalty share of `amount` from `payer` to the configured recipient
in `token`; in accrual mode it credits the `(collection, token)` ledger. It
records the settlement totals and returns the seller's net after royalties —
the seller is **not** paid here. It is for callers that handle the underlying
sale/payment outside `settle_sale` and only need the royalty leg settled; a
`Disabled` or zero-bps configuration transfers nothing and returns the full
`amount`. It requires the collection's and the payer's authorization, and
emits the same `SaleSettled` event as the settlement entrypoints.

### Accrual / recoupment ledger

When `accrual` is `true` on a collection's royalty configuration, every sale's
royalty share is credited to a per-`(collection, token)` accrual ledger held
in contract custody instead of paid out immediately. The ledger is bumped
on every credit and on every successful sweep, so it is never evictable
mid-accrual as long as sales continue.

- `get_accrued(collection, token)` returns the total accrued balance for
  that token.
- `get_recipient_accrued(collection, token, recipient)` returns the amount
  credited to a specific recipient (with the current single-recipient
  configuration this equals `get_accrued`).
- `distribute_accrued(collection, token)` pays the full accrued balance to
  the configured recipient and clears the ledger. It is authorized by the
  collection. The ledger is debited before the outbound transfer; if the
  transfer fails the whole invocation rolls back and the ledger is restored,
  preventing a double sweep. A sweep with no accrued balance returns
  `ForgeError::AccrualEmpty`.

Conservation across the two modes: in payout mode the sum of all
`royalty_share` amounts transferred equals `SettlementSummary.royalties_paid`;
in accrual mode the sum of all credits equals the amount later swept, and
`SettlementSummary.royalties_paid` is updated only on sweep.

### Sale quotes

`quote_sale(collection, amount)` is a read-only view returning the exact
split a settlement of `amount` would apply, as a `SaleQuote { gross,
royalty_bps, royalty_amount, seller_net }`. Settle-parity guarantee: the
quote runs the same validation order and the same derivation as the
settlement entrypoints — configuration load (`NotFound` for an unregistered
collection), `amount > 0` (`InvalidInput`, mirroring `distribute` and
`settle_sale`), then the same `effective_bps` + `split` resolution — so the
returned numbers are the settlement's own, floor rounding included, and
`royalty_amount + seller_net == gross` exactly. A `Disabled` configuration
quotes at zero bps, matching `settle_sale`'s settle-in-full behavior. The
quote never mutates storage, requires no authorization, and emits no events.
It is the per-sale counterpart of `get_settlement_summary` and exists so a
marketplace UI can display "you will pay X, royalty is Y, seller receives
Z" from the contract's own math instead of a parallel off-chain
implementation.

### Batch settlement

`settle_sales` settles a batch of sales of one collection in one invocation
against one payer authorization, with per-sale semantics identical to
`settle_sale`:

1. Load the configuration (`NotFound` if unregistered), then validate the
   whole batch before any token moves: `sales` must be non-empty and at
   most `MAX_SETTLE_SALES` (20) long, and every `amount` must be positive
   (`InvalidInput` for either). The cap bounds one transaction's worst case
   to at most `2 * MAX_SETTLE_SALES` nested token transfers, keeping a
   batched settlement inside Soroban's per-transaction instruction budget
   and ledger bandwidth; larger sets issue several calls, each still
   atomic.
2. Require the collection's and the payer's authorization once — the
   payer's single `require_auth` covers every nested token transfer in the
   batch.
3. Compute each sale's split and the batch aggregate with checked
   arithmetic, then stage the updated settlement totals by adding the
   aggregate to the stored summary (`ArithmeticOverflow` — checked against
   the existing totals, still before any transfer).
4. Transfer each sale in order, seller first and royalty recipient last
   (or into contract custody in accrual mode), skipping the royalty
   transfer when the share floors to zero — exactly the `settle_sale`
   order, sale after sale.
5. Only after every transfer succeeds, commit the summary once with the
   batch's aggregate deltas. In accrual mode the aggregate royalty credit
   is also written to the `(collection, token)` ledger exactly once after
   the transfers, and the per-sale `AccrualCredited` events are emitted.
   The per-sale `Settlement`s are returned in sale order.

Atomicity is all-or-nothing for the batch: a failure in any sale —
including a later sale's transfer after earlier sales fully succeeded —
rolls the whole invocation back, so balances, the ledger, and the summary
are exactly as they were before the call (no sale is half-settled).

## Compatibility

`set_royalty` now takes an `accrual` flag: `set_royalty(collection,
recipient, bps, accrual)`. This is a breaking change for callers and the
generated TypeScript ABI; existing registrations default to payout mode
(`accrual=false`). `get_royalty` returns the new `accrual` field. `distribute`,
`settle_sale`, and `settle_sales` honor the flag transparently.
`distribute_accrued`, `get_accrued`, and `get_recipient_accrued` are new
additive entrypoints. The generated
`SorobanForgeMarketplaceRoyaltiesClient` gains all new methods and types
automatically, and `Settlement`/`SettlementSummary` are shared by the
settlement entrypoints.

## Storage & TTL Maintenance

Persistent storage:
- `Royalty` configuration records (`DataKey::Royalty(Address)`),
- `SettlementSummary` records (`DataKey::Summary(Address)`),
- `AccrualLedger` records per `(collection, token)`
  (`DataKey::AccrualLedger(AccrualLedgerKey(collection, token))`).

`set_royalty`, `distribute`, `settle_sale`, `settle_sales`, and
`distribute_accrued` extend persistent storage TTL on every write to a 30-day
horizon (`30 * DAY_IN_LEDGERS = 518,400` ledgers). The accrual ledger is
bumped on every credit and on every successful sweep, so it is never
evictable mid-accrual as long as sales continue.

A permissionless public keeper entrypoint `touch_ttl(collection)` allows
anyone to bump persistent storage TTL for a collection's `Royalty` and
`Summary` records. If no royalty configuration exists for `collection`,
`touch_ttl` returns `ForgeError::NotFound`.

## Events

The contract emits typed on-chain lifecycle events for indexers and off-chain monitoring:

- `RoyaltyConfigured` (topic: `collection: Address`) — emitted when a royalty configuration is registered or updated via `set_royalty`. Contains `recipient`, `bps`, and `accrual`.
- `SaleSettled` (topic: `collection: Address`) — emitted on sale settlement via `settle_sale` or `settle_sales`. Contains `token`, `payer`, `seller`, `royalty_recipient`, `gross_amount`, `seller_net`, and `royalty_share`.
- `AccrualCredited` (topic: `collection: Address`) — emitted in accrual mode when a sale's royalty share is credited to the contract ledger. Contains `token`, `payer`, `seller`, `recipient`, and `amount`.
- `AccrualDistributed` (topic: `collection: Address`) — emitted when `distribute_accrued` pays the accrued balance to the recipient. Contains `token`, `recipient`, and `amount`.

- `RoyaltyConfigured` (topic: `collection: Address`) — emitted when a royalty configuration is registered or updated via `set_royalty`. Contains `recipient` and `bps`.
- `SaleSettled` (topic: `collection: Address`) — emitted once per settled sale via `settle_sale` or normal-size `settle_sales` batches (including zero-share and disabled-royalty sales). A batch larger than ten sales emits one aggregate event to remain within Soroban's event-size budget; its `seller` is the collection sentinel and its amount fields are batch totals, while the return value retains per-sale details. A failed batch emits no contract events because the invocation rolls back. Contains `token`, `payer`, `seller`, `royalty_recipient`, `gross_amount`, `seller_net`, and `royalty_share`.
