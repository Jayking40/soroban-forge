//! Randomized invariant suite (proptest) for marketplace royalties.
//!
//! Exercised invariants:
//! 1. Split conservation: `royalty_share + returned_net == amount` for every `amount > 0` and `bps <= 10_000`.
//! 2. Rate boundaries: `bps == 0` returns the full amount and `bps == 10_000` returns zero net.
//! 3. Bounded net: returned net is never negative and never exceeds the amount.
//! 4. Batch conservation: a `settle_sales` batch settles exactly like the same
//!    sales driven through `settle_sale` one at a time — identical per-sale
//!    splits in sale order, one summary committed as the batch's aggregate
//!    deltas, and every balance at its per-sale share — over batch shapes from
//!    1 to `MAX_SETTLE_SALES`, with duplicate sellers and zero-share sales
//!    included.
//! 5. Batch failure isolation: a batch carrying exactly one defect — a
//!    non-positive amount, an unconfigured collection, or a deliberately
//!    underfunded payer — fails with the same error the single-sale path
//!    raises for that defect and leaves every balance and the committed
//!    summary byte-identical to the pre-call state.
//! 6. Accrual conservation: in accrual mode, `settle_sale` credits sum to the
//!    ledger and `distribute_accrued` pays exactly that sum, leaving zero
//!    custody and updating `royalties_paid` by the swept amount.
//!
//! Earlier payout-mode invariants (kept in the suite above):
//! - Batch aggregate: `sum(seller_net) + sum(royalty_share) == sum(amount)`.
//! - Cap boundary: a batch exceeding `MAX_SETTLE_SALES` is rejected before any transfer.
//! - Rollback integrity: a failed batch transfer leaves every balance and the summary untouched.

use crate::{
    MarketplaceRoyalties, SettlementSummary, SorobanForgeMarketplaceRoyaltiesClient,
    MAX_SETTLE_SALES,
};
use proptest::collection::vec as prop_vec;
use proptest::prelude::*;
use soroban_forge_shared_utils::ForgeError;
use soroban_sdk::testutils::Address as _;
use soroban_sdk::token::{Client as TokenClient, StellarAssetClient};
use soroban_sdk::{Address, Env};
use std::vec::Vec;

const MAX_AMOUNT: i128 = 1_000_000_000_000_000;

/// Distinct sellers in the batch pool. Index 0 doubles as the single-sale
/// `seller` the existing properties settle through.
const SELLER_POOL: usize = 4;

struct World {
    env: Env,
    token: Address,
    contract_id: Address,
    collection: Address,
    recipient: Address,
    seller: Address,
    /// Seller pool for batch plans; `sellers[0]` is `seller`, so batch
    /// indices and the single-sale properties address the same accounts.
    sellers: std::vec::Vec<Address>,
    payer: Address,
}

fn setup_world(bps: u32, mint_amount: i128) -> World {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin);
    let token = sac.address();

    let collection = Address::generate(&env);
    let recipient = Address::generate(&env);
    let seller = Address::generate(&env);
    let payer = Address::generate(&env);
    let mut sellers = std::vec::Vec::with_capacity(SELLER_POOL);
    sellers.push(seller.clone());
    for _ in 1..SELLER_POOL {
        sellers.push(Address::generate(&env));
    }

    StellarAssetClient::new(&env, &token).mint(&payer, &mint_amount);

    let contract_id = env.register(MarketplaceRoyalties, ());
    let client = SorobanForgeMarketplaceRoyaltiesClient::new(&env, &contract_id);
    client.set_royalty(&collection, &recipient, &bps, &false);

    World {
        env,
        token,
        contract_id,
        collection,
        recipient,
        seller,
        sellers,
        payer,
    }
}

impl World {
    fn client(&self) -> SorobanForgeMarketplaceRoyaltiesClient<'_> {
        SorobanForgeMarketplaceRoyaltiesClient::new(&self.env, &self.contract_id)
    }

    fn token_client(&self) -> TokenClient<'_> {
        TokenClient::new(&self.env, &self.token)
    }

    fn balance(&self, address: &Address) -> i128 {
        self.token_client().balance(address)
    }

    /// Balances of every address that can hold this token in the batch
    /// properties — payer, royalty recipient, the whole seller pool, and the
    /// contract — in a fixed order, so a snapshot taken before a call can be
    /// compared byte-for-byte with one taken after it.
    fn balances(&self) -> std::vec::Vec<i128> {
        let mut addresses = std::vec::Vec::with_capacity(3 + self.sellers.len());
        addresses.push(self.payer.clone());
        addresses.push(self.recipient.clone());
        addresses.extend(self.sellers.iter().cloned());
        addresses.push(self.contract_id.clone());
        addresses
            .iter()
            .map(|address| self.balance(address))
            .collect()
    }

    /// The committed summary for `collection`, or `None` while it has never
    /// settled — the read-only view's `NotFound`, as an option.
    fn summary(&self, collection: &Address) -> Option<SettlementSummary> {
        self.client()
            .try_get_settlement_summary(collection)
            .ok()
            .and_then(|result| result.ok())
    }

    /// Map a `(seller index, amount)` plan onto this world's seller pool as
    /// one `settle_sales` batch vector.
    fn batch(&self, plan: &[(u32, i128)]) -> soroban_sdk::Vec<(Address, i128)> {
        let mut sales = soroban_sdk::Vec::new(&self.env);
        for (seller, amount) in plan {
            let seller = &self.sellers[(*seller as usize) % self.sellers.len()];
            sales.push_back((seller.clone(), *amount));
        }
        sales
    }
}

// -----------------------------------------------------------------------
// Strategies
// -----------------------------------------------------------------------

/// The two rate boundaries weighted against the mid-range: `0` makes every
/// sale a zero-royalty-share sale (recipient transfer skipped), `10_000`
/// makes every sale a zero-seller-net sale (seller transfer skipped).
fn bps_strategy() -> impl Strategy<Value = u32> {
    prop_oneof![Just(0u32), Just(10_000_u32), 1u32..=10_000_u32]
}

/// Sale amounts: the tiny branch floors to a zero royalty share at most
/// rates, so skipped-recipient sales ride along inside ordinary batches;
/// the upper branches reach the suite's `MAX_AMOUNT` ceiling.
fn amount_strategy() -> impl Strategy<Value = i128> {
    prop_oneof![1i128..=50, 1i128..=1_000_000, 1i128..=MAX_AMOUNT]
}

/// Batch size: the endpoints — one sale and a batch at
/// `MAX_SETTLE_SALES` — pinned beside the uniform middle, so every run
/// covers both ends of `1..=MAX_SETTLE_SALES` without biasing the average.
fn batch_size() -> impl Strategy<Value = usize> {
    prop_oneof![
        Just(1usize),
        Just(MAX_SETTLE_SALES as usize),
        1usize..=(MAX_SETTLE_SALES as usize),
    ]
}

/// A random batch shape: `(seller index, amount)` entries with indices drawn
/// with replacement, so duplicate sellers across one batch are the common
/// case rather than an edge case.
fn sales_plan() -> impl Strategy<Value = std::vec::Vec<(u32, i128)>> {
    batch_size().prop_flat_map(|size| {
        prop::collection::vec((0u32..(SELLER_POOL as u32), amount_strategy()), size..=size)
    })
}

struct BatchWorld {
    env: Env,
    token: Address,
    contract_id: Address,
    collection: Address,
    payer: Address,
    sellers: Vec<Address>,
    _recipient: Address,
    amounts: Vec<i128>,
}

fn setup_batch_world(bps: u32, amounts: Vec<i128>) -> BatchWorld {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin);
    let token = sac.address();

    let collection = Address::generate(&env);
    let recipient = Address::generate(&env);
    let payer = Address::generate(&env);

    let total: i128 = amounts.iter().sum();
    StellarAssetClient::new(&env, &token).mint(&payer, &total);

    let sellers: Vec<Address> = (0..amounts.len())
        .map(|_| Address::generate(&env))
        .collect();

    let contract_id = env.register(MarketplaceRoyalties, ());
    let client = SorobanForgeMarketplaceRoyaltiesClient::new(&env, &contract_id);
    client.set_royalty(&collection, &recipient, &bps, &false);

    BatchWorld {
        env,
        token,
        contract_id,
        collection,
        payer,
        sellers,
        _recipient: recipient,
        amounts,
    }
}

impl BatchWorld {
    fn client(&self) -> SorobanForgeMarketplaceRoyaltiesClient<'_> {
        SorobanForgeMarketplaceRoyaltiesClient::new(&self.env, &self.contract_id)
    }

    fn batch(&self) -> soroban_sdk::Vec<(Address, i128)> {
        let mut batch = soroban_sdk::Vec::new(&self.env);
        for (seller, amount) in self.sellers.iter().zip(self.amounts.iter()) {
            batch.push_back((seller.clone(), *amount));
        }
        batch
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn prop_split_conservation_and_bounds(
        amount in 1i128..=MAX_AMOUNT,
        bps in 0u32..=10_000_u32,
    ) {
        let w = setup_world(bps, amount);
        let net = w
            .client()
            .distribute(&w.collection, &w.token, &w.payer, &w.seller, &amount);

        let expected_royalty = amount * (bps as i128) / 10_000;

        prop_assert_eq!(net + expected_royalty, amount, "split conservation: net + royalty must equal total amount");
        prop_assert!(net >= 0, "seller net must never be negative");
        prop_assert!(net <= amount, "seller net must never exceed gross amount");
        prop_assert!(expected_royalty >= 0, "royalty share must never be negative");
        prop_assert!(expected_royalty <= amount, "royalty share must never exceed gross amount");
    }

    #[test]
    fn prop_boundary_rates(
        amount in 1i128..=MAX_AMOUNT,
    ) {
        let w_zero = setup_world(0, amount);
        let net_zero = w_zero
            .client()
            .distribute(&w_zero.collection, &w_zero.token, &w_zero.payer, &w_zero.seller, &amount);
        prop_assert_eq!(net_zero, amount, "bps == 0 must return full amount");

        let w_full = setup_world(10_000, amount);
        let net_full = w_full
            .client()
            .distribute(&w_full.collection, &w_full.token, &w_full.payer, &w_full.seller, &amount);
        prop_assert_eq!(net_full, 0, "bps == 10_000 must return 0 net");
    }

    #[test]
    fn prop_atomic_settlement_conservation(
        amount in 1i128..=MAX_AMOUNT,
        bps in 0u32..=10_000_u32,
    ) {
        let w = setup_world(bps, amount);
        let settlement = w.client().settle_sale(&w.collection, &w.token, &w.payer, &w.seller, &amount);

        prop_assert_eq!(settlement.royalty_share + settlement.seller_net, amount, "settled shares must sum to amount");
        prop_assert!(settlement.seller_net >= 0);
        prop_assert!(settlement.royalty_share >= 0);
    }

    #[test]
    fn prop_batch_conservation(
        bps in 0u32..=10_000_u32,
        amounts in prop_vec(1i128..=MAX_AMOUNT, 1..=MAX_SETTLE_SALES as usize),
    ) {
        let w = setup_batch_world(bps, amounts.clone());
        let settled = w.client().settle_sales(&w.collection, &w.token, &w.payer, &w.batch());

        let mut sum_seller_net: i128 = 0;
        let mut sum_royalty_share: i128 = 0;
        let mut sum_amounts: i128 = 0;
        for (i, sale) in settled.iter().enumerate() {
            sum_seller_net += sale.seller_net;
            sum_royalty_share += sale.royalty_share;
            sum_amounts += amounts[i];
            prop_assert_eq!(sale.seller_net + sale.royalty_share, amounts[i], "per-sale conservation: seller_net + royalty_share must equal amount");
        }

        prop_assert_eq!(sum_seller_net + sum_royalty_share, sum_amounts, "batch conservation: sum(seller_net) + sum(royalty_share) must equal sum(amount)");

        let summary = w.client().get_settlement_summary(&w.collection);
        prop_assert_eq!(summary.sales, amounts.len() as u32, "summary sales count must match batch size");
        prop_assert_eq!(summary.gross_volume, sum_amounts, "summary gross_volume must equal total amount");
        prop_assert_eq!(summary.royalties_paid, sum_royalty_share, "summary royalties_paid must equal total royalty share");
    }

    #[test]
    fn prop_batch_cap_boundary(
        bps in 0u32..=10_000_u32,
    ) {
        let n = MAX_SETTLE_SALES as usize + 1;
        let amounts = (0..n).map(|_| 1i128).collect::<Vec<_>>();
        let w = setup_batch_world(bps, amounts);

        // try_settle_sales returns Result<Result<Vec<Settlement>, ConversionError>, Result<ForgeError, InvokeError>>.
        // A cap rejection surfaces as an outer Err containing the contract's Err(ForgeError::InvalidInput).
        let result = w.client().try_settle_sales(&w.collection, &w.token, &w.payer, &w.batch());
        let err = result.unwrap_err().unwrap();
        prop_assert_eq!(err, ForgeError::InvalidInput, "batch over cap must return InvalidInput");

        let tc = TokenClient::new(&w.env, &w.token);
        prop_assert_eq!(tc.balance(&w.payer), n as i128, "payer balance must be untouched on cap rejection");

        let summary_result = w.client().try_get_settlement_summary(&w.collection);
        let summary_err = summary_result.unwrap_err().unwrap();
        prop_assert_eq!(summary_err, ForgeError::NotFound, "summary must not exist after cap rejection");
    }

    #[test]
    fn prop_batch_rollback_integrity(
        bps in 0u32..=10_000_u32,
    ) {
        // Build a world where the payer has enough for one sale but not the batch.
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let sac = env.register_stellar_asset_contract_v2(admin);
        let token = sac.address();
        let collection = Address::generate(&env);
        let recipient = Address::generate(&env);
        let payer = Address::generate(&env);
        let seller_a = Address::generate(&env);
        let seller_b = Address::generate(&env);

        // Mint only one sale's worth — the batch has two sales so the second transfer fails.
        StellarAssetClient::new(&env, &token).mint(&payer, &1_000_000_000_000_000_i128);

        let contract_id = env.register(MarketplaceRoyalties, ());
        let client = SorobanForgeMarketplaceRoyaltiesClient::new(&env, &contract_id);
        client.set_royalty(&collection, &recipient, &bps, &false);

        let mut batch = soroban_sdk::Vec::new(&env);
        batch.push_back((seller_a.clone(), 1_000_000_000_000_000_i128));
        batch.push_back((seller_b.clone(), 1_000_000_000_000_000_i128));

        let payer_balance_before = StellarAssetClient::new(&env, &token).balance(&payer);
        let recipient_balance_before = StellarAssetClient::new(&env, &token).balance(&recipient);
        let seller_a_balance_before = StellarAssetClient::new(&env, &token).balance(&seller_a);
        let seller_b_balance_before = StellarAssetClient::new(&env, &token).balance(&seller_b);
        let contract_balance_before = StellarAssetClient::new(&env, &token).balance(&contract_id);

        let result = client.try_settle_sales(&collection, &token, &payer, &batch);
        let err = result.unwrap_err().unwrap();
        prop_assert_eq!(err, ForgeError::TokenTransferFailed, "a batch with insufficient balance must fail on transfer");

        // Verify every balance and the summary are untouched by frame rollback.
        prop_assert_eq!(StellarAssetClient::new(&env, &token).balance(&payer), payer_balance_before, "payer balance must be restored on rollback");
        prop_assert_eq!(StellarAssetClient::new(&env, &token).balance(&recipient), recipient_balance_before, "recipient balance must be restored on rollback");
        prop_assert_eq!(StellarAssetClient::new(&env, &token).balance(&seller_a), seller_a_balance_before, "seller_a balance must be restored on rollback");
        prop_assert_eq!(StellarAssetClient::new(&env, &token).balance(&seller_b), seller_b_balance_before, "seller_b balance must be restored on rollback");
        prop_assert_eq!(StellarAssetClient::new(&env, &token).balance(&contract_id), contract_balance_before, "contract balance must be restored on rollback");

        let summary_after = client.try_get_settlement_summary(&collection);
        let summary_after_err = summary_after.unwrap_err().unwrap();
        prop_assert_eq!(summary_after_err, ForgeError::NotFound, "summary must be untouched on rollback");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// A random `settle_sales` batch must settle exactly like the same sales
    /// driven through `settle_sale` one at a time: identical per-sale splits
    /// in sale order, one summary committed as the batch's aggregate deltas,
    /// and every balance at its per-sale share. Both worlds mint the payer
    /// exactly `sum(amounts)`, so no case can fail on funding by accident.
    #[test]
    fn prop_batch_conservation_matches_single_sale(
        bps in bps_strategy(),
        plan in sales_plan(),
    ) {
        let total: i128 = plan.iter().map(|(_, amount)| amount).sum();
        let batch_world = setup_world(bps, total);
        let single_world = setup_world(bps, total);

        // World A: the whole batch in one invocation.
        let batch_settled = batch_world.client().settle_sales(
            &batch_world.collection,
            &batch_world.token,
            &batch_world.payer,
            &batch_world.batch(&plan),
        );

        // World B: the same sales, one `settle_sale` at a time.
        let mut single_settled = soroban_sdk::Vec::new(&single_world.env);
        for (seller, amount) in &plan {
            let seller =
                &single_world.sellers[(*seller as usize) % single_world.sellers.len()];
            single_settled.push_back(single_world.client().settle_sale(
                &single_world.collection,
                &single_world.token,
                &single_world.payer,
                seller,
                amount,
            ));
        }

        prop_assert_eq!(
            batch_settled.len(),
            plan.len() as u32,
            "one settlement per sale, in sale order"
        );

        // Independent floor oracle — the split math both entrypoints share,
        // recomputed here so the batch cannot redefine its own expectations.
        let mut expected_nets: std::vec::Vec<i128> =
            std::iter::repeat_n(0i128, single_world.sellers.len()).collect();
        let mut expected_royalties: i128 = 0;
        for (i, (seller, amount)) in plan.iter().enumerate() {
            let amount = *amount;
            let settlement = batch_settled.get(i as u32).expect("one settlement per sale");
            let royalty_share = amount * (bps as i128) / 10_000;
            let seller_net = amount - royalty_share;
            prop_assert_eq!(
                settlement.royalty_share + settlement.seller_net,
                amount,
                "a batch sale must conserve its amount exactly"
            );
            prop_assert_eq!(
                settlement.royalty_share,
                royalty_share,
                "batch share must equal the single-sale floor split"
            );
            prop_assert_eq!(
                settlement.seller_net,
                seller_net,
                "batch net must equal the single-sale remainder"
            );
            let index = (*seller as usize) % expected_nets.len();
            expected_nets[index] += seller_net;
            expected_royalties += royalty_share;
        }

        // The batch's per-sale splits match the single-sale path exactly.
        prop_assert_eq!(
            batch_settled,
            single_settled,
            "batch splits must match settle_sale one-for-one, in sale order"
        );

        // Every balance in both worlds moves by exactly its per-sale share.
        for w in [&batch_world, &single_world] {
            prop_assert_eq!(w.balance(&w.payer), 0, "the payer funds exactly the batch");
            prop_assert_eq!(
                w.balance(&w.recipient),
                expected_royalties,
                "the recipient receives the sum of the per-sale royalty shares"
            );
            prop_assert_eq!(
                w.balance(&w.contract_id),
                0,
                "the contract settles through and never retains funds"
            );
            for (seller, expected) in w.sellers.iter().zip(&expected_nets) {
                prop_assert_eq!(
                    w.balance(seller),
                    *expected,
                    "each seller receives the sum of its per-sale nets"
                );
            }
        }

        // Committed exactly once, as the batch's aggregates — and identical
        // to what the same sales commit through `settle_sale`.
        let batch_summary = batch_world.summary(&batch_world.collection);
        let single_summary = single_world.summary(&single_world.collection);
        prop_assert_eq!(
            &batch_summary,
            &single_summary,
            "batch summary must match the single-sale path"
        );
        prop_assert_eq!(
            &batch_summary,
            &Some(SettlementSummary {
                sales: plan.len() as u32,
                gross_volume: total,
                royalties_paid: expected_royalties,
            }),
            "the one committed summary equals the batch's aggregate deltas"
        );
    }

    /// Exactly one defect injected into an otherwise valid batch — a zero
    /// amount, a negative amount, an unconfigured collection, or a payer
    /// deliberately funded for every sale but the last — must fail with the
    /// same error the single-sale path raises for that defect, leaving every
    /// balance and the committed summary byte-identical to the pre-call
    /// state. A warm-up sale gives the pre-call state a real summary and
    /// non-zero balances, so "unchanged" is a non-trivial demand. Cap-boundary
    /// and random-position rollback variants belong to #176, not here.
    #[test]
    fn prop_batch_failure_isolation(
        bps in bps_strategy(),
        plan in sales_plan(),
        fault in 0u8..=3,
        position in 0u32..=MAX_SETTLE_SALES,
    ) {
        // Warm-up sale, settled before the snapshot: the summary and the
        // balances under protection are real, not "still NotFound / zero".
        const WARM_UP: i128 = 100;

        let total: i128 = plan.iter().map(|(_, amount)| amount).sum();
        // Fault 3 (underfunded) funds every sale but the last, so the batch
        // must roll back `len - 1` fully settled sales. The validation faults
        // are funded in full: their failure can only come from the injected
        // defect, never from an empty payer by accident.
        let funding = match fault {
            3 => total - plan[plan.len() - 1].1,
            _ => total,
        };

        let w = setup_world(bps, WARM_UP + funding);
        w.client().settle_sale(
            &w.collection,
            &w.token,
            &w.payer,
            &w.sellers[0],
            &WARM_UP,
        );

        // "Unknown config": this contract keys configuration by collection,
        // so an unconfigured collection is the batch's invalid element — a
        // per-seller config has no counterpart in this iteration.
        let unconfigured = Address::generate(&w.env);
        let collection = if fault == 2 { &unconfigured } else { &w.collection };

        // Exactly one invalid element, at a random position in the batch.
        let mut plan = plan;
        if fault == 0 || fault == 1 {
            let i = (position as usize) % plan.len();
            plan[i].1 = if fault == 0 { 0 } else { -plan[i].1 };
        }

        let before_balances = w.balances();
        let before_summary = w.summary(&w.collection);
        let before_unconfigured_summary = w.summary(&unconfigured);

        let expected = match fault {
            0 | 1 => ForgeError::InvalidInput,
            2 => ForgeError::NotFound,
            _ => ForgeError::TokenTransferFailed,
        };

        let batch_err = w
            .client()
            .try_settle_sales(collection, &w.token, &w.payer, &w.batch(&plan))
            .unwrap_err()
            .unwrap();
        prop_assert_eq!(batch_err, expected, "the defective batch must fail");
        prop_assert_eq!(
            &w.balances(),
            &before_balances,
            "a failed batch moves no balance"
        );
        prop_assert_eq!(
            &w.summary(&w.collection),
            &before_summary,
            "a failed batch leaves the summary untouched"
        );
        prop_assert_eq!(
            &w.summary(&unconfigured),
            &before_unconfigured_summary,
            "an unconfigured collection gains no summary either"
        );

        // The same defect through `settle_sale` must raise the same error —
        // and it too must leave the world exactly as it found it.
        let single_err = match fault {
            0 | 1 => {
                let (_, amount) = plan[(position as usize) % plan.len()];
                w.client().try_settle_sale(
                    &w.collection,
                    &w.token,
                    &w.payer,
                    &w.sellers[0],
                    &amount,
                )
            }
            2 => w.client().try_settle_sale(
                &unconfigured,
                &w.token,
                &w.payer,
                &w.sellers[0],
                &WARM_UP,
            ),
            // Underfunded: one sale costing a token more than the payer
            // holds — the single-sale twin of "all but the last sale".
            _ => w.client().try_settle_sale(
                &w.collection,
                &w.token,
                &w.payer,
                &w.sellers[0],
                &(funding + 1),
            ),
        }
        .unwrap_err()
        .unwrap();
        prop_assert_eq!(
            single_err, expected,
            "settle_sale raises the same error for the same defect"
        );
        prop_assert_eq!(
            &w.balances(),
            &before_balances,
            "the single-sale cross-check moves no balance either"
        );
        prop_assert_eq!(
            &w.summary(&w.collection),
            &before_summary,
            "the single-sale cross-check leaves the summary untouched"
        );
    }
}

/// Accrual-mode world: same shape as `setup_world` but the collection is
/// opted into accrual and the contract address is exposed so custody checks
/// can inspect it.
fn setup_accrual_world(bps: u32, mint_amount: i128) -> World {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin);
    let token = sac.address();

    let collection = Address::generate(&env);
    let recipient = Address::generate(&env);
    let seller = Address::generate(&env);
    let payer = Address::generate(&env);
    let mut sellers = std::vec::Vec::with_capacity(SELLER_POOL);
    sellers.push(seller.clone());
    for _ in 1..SELLER_POOL {
        sellers.push(Address::generate(&env));
    }

    StellarAssetClient::new(&env, &token).mint(&payer, &mint_amount);

    let contract_id = env.register(MarketplaceRoyalties, ());
    let client = SorobanForgeMarketplaceRoyaltiesClient::new(&env, &contract_id);
    client.set_royalty(&collection, &recipient, &bps, &true);

    World {
        env,
        token,
        contract_id,
        collection,
        recipient,
        seller,
        sellers,
        payer,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// In accrual mode, the sum of credited royalty shares equals the
    /// amount later swept, with zero contract custody left behind and the
    /// summary's `royalties_paid` incremented only on sweep.
    #[test]
    fn prop_accrual_credit_equals_sweep_payout(
        bps in 0u32..=10_000_u32,
        amount in 1i128..=MAX_AMOUNT,
    ) {
        let w = setup_accrual_world(bps, amount);
        let expected_royalty = amount * (bps as i128) / 10_000;
        let expected_net = amount - expected_royalty;

        w.client().settle_sale(&w.collection, &w.token, &w.payer, &w.seller, &amount);

        prop_assert_eq!(
            w.token_client().balance(&w.contract_id),
            expected_royalty,
            "contract custody equals the credited royalty share"
        );
        prop_assert_eq!(
            w.token_client().balance(&w.recipient),
            0,
            "recipient is not paid until sweep"
        );
        prop_assert_eq!(
            w.token_client().balance(&w.seller),
            expected_net,
            "seller receives net"
        );
        prop_assert_eq!(
            w.client().get_accrued(&w.collection, &w.token),
            expected_royalty,
            "ledger view equals credited amount"
        );

        let summary_before = w.client()
            .get_settlement_summary(&w.collection);
        prop_assert_eq!(
            summary_before.royalties_paid, 0,
            "royalties_paid stays zero until sweep"
        );

        let swept = w.client().distribute_accrued(&w.collection, &w.token);
        prop_assert_eq!(swept, expected_royalty);
        prop_assert_eq!(
            w.token_client().balance(&w.contract_id),
            0,
            "contract custody is empty after sweep"
        );
        prop_assert_eq!(
            w.token_client().balance(&w.recipient),
            expected_royalty,
            "recipient receives exact accrued amount"
        );
        prop_assert_eq!(
            w.client().get_accrued(&w.collection, &w.token),
            0,
            "ledger is cleared after sweep"
        );
        let summary_after = w.client()
            .get_settlement_summary(&w.collection);
        prop_assert_eq!(
            summary_after.royalties_paid,
            expected_royalty,
            "summary records the swept amount"
        );
    }
}
