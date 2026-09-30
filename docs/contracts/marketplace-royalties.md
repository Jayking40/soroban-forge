# Marketplace Royalties Contract

NFT / digital asset sales with configurable royalty distribution across
secondary sales — distributed by `distribute` (real SEP-41 royalty payout)
and settled atomically in real SEP-41 tokens by `settle_sale` (one sale) or
`settle_sales` (a capped, all-or-nothing batch).

## Interface

```rust
fn set_royalty(collection, recipient, bps) -> Result<(), ForgeError>
fn distribute(collection, token, payer, seller, amount) -> Result<i128, ForgeError>
fn settle_sale(collection, token, payer, seller, amount) -> Result<Settlement, ForgeError>
fn settle_sales(collection, token, payer, sales: Vec<(seller, amount)>) -> Result<Vec<Settlement>, ForgeError>
fn get_royalty(collection) -> Result<Royalty, ForgeError>
fn get_settlement_summary(collection) -> Result<SettlementSummary, ForgeError>
fn quote_sale(collection, amount) -> Result<SaleQuote, ForgeError>
fn touch_ttl(collection) -> Result<(), ForgeError>
```

## Concepts

- **Creators** receive a split of every sale, paid to the configured
  `recipient` on settlement.
- **Sellers** receive the net of every sale.
- **Payers** fund both transfers from their `token` balance in a single
  authorized call.

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
3. Transfer `seller_net` from `payer` to `seller`, then `royalty_share` from
   `payer` to the configured `recipient` — the recipient is paid last.
4. Only after both transfers succeed, commit the collection's cumulative
   settlement totals and return the `Settlement`.

A `Disabled` or zero-bps configuration skips the recipient transfer and
settles the full amount to the seller. Token failures (insufficient balance,
missing trustline, undeployed token) surface as
`ForgeError::TokenTransferFailed`, and any returned error rolls back the
whole invocation — a failed settlement can never leave the recipient
partially paid and never commits totals.

`distribute` is a standalone royalty settlement: it transfers the royalty
share of `amount` (`amount * bps / 10_000`, floored) from `payer` to the
configured recipient in `token`, records the settlement totals, and returns
the seller's net after royalties — the seller is **not** paid here. It is
for callers that handle the underlying sale/payment outside `settle_sale`
and only need the royalty leg settled; a `Disabled` or zero-bps
configuration transfers nothing and returns the full `amount`. It requires
the collection's and the payer's authorization, and emits the same
`SaleSettled` event as the settlement entrypoints.

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
4. Transfer each sale in order, seller first and royalty recipient last,
   skipping the recipient transfer when the share floors to zero — exactly
   the `settle_sale` order, sale after sale.
5. Only after every transfer succeeds, commit the summary once with the
   batch's aggregate deltas (`sales`, `gross_volume`, `royalties_paid`) and
   return the per-sale `Settlement`s in sale order.

Atomicity is all-or-nothing for the batch: a failure in any sale —
including a later sale's transfer after earlier sales fully succeeded —
rolls the whole invocation back, so balances and the summary are exactly as
they were before the call (no sale is half-settled).

## Compatibility

`set_royalty` and `get_royalty` are unchanged. `distribute` now takes the
full settlement context `(collection, token, payer, seller, amount)` — the
same parameter order as `settle_sale` — and pays the royalty share instead
of computing it. `settle_sale`, `settle_sales`, and
`get_settlement_summary` are additive; the generated
`SorobanForgeMarketplaceRoyaltiesClient` gains all three automatically, and
`Settlement`/`SettlementSummary` are shared by both settlement
entrypoints.

## Storage & TTL Maintenance

Persistent storage: `Royalty` configuration records (`DataKey::Royalty(Address)`) and `SettlementSummary` records (`DataKey::Summary(Address)`).

`set_royalty`, `distribute`, `settle_sale`, and `settle_sales` extend
persistent storage TTL on every write to a 30-day horizon
(`30 * DAY_IN_LEDGERS = 518,400` ledgers).

A permissionless public keeper entrypoint `touch_ttl(collection)` allows anyone to bump persistent storage TTL for a collection's `Royalty` and `Summary` records. If no royalty configuration exists for `collection`, `touch_ttl` returns `ForgeError::NotFound`.

## Events

The contract emits typed on-chain lifecycle events for indexers and off-chain monitoring:

- `RoyaltyConfigured` (topic: `collection: Address`) — emitted when a royalty configuration is registered or updated via `set_royalty`. Contains `recipient` and `bps`.
- `SaleSettled` (topic: `collection: Address`) — emitted once per settled sale via `settle_sale` or normal-size `settle_sales` batches (including zero-share and disabled-royalty sales). A batch larger than ten sales emits one aggregate event to remain within Soroban's event-size budget; its `seller` is the collection sentinel and its amount fields are batch totals, while the return value retains per-sale details. A failed batch emits no contract events because the invocation rolls back. Contains `token`, `payer`, `seller`, `royalty_recipient`, `gross_amount`, `seller_net`, and `royalty_share`.
