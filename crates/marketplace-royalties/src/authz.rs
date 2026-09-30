//! Negative authorization tests for marketplace royalties.
//!
//! Authorization model:
//! - `set_royalty` requires the collection.
//! - `distribute` requires the collection and the payer (the payer's
//!   authorization covers the nested royalty transfer to the recipient).
//! - `settle_sale` requires the collection and the payer.

use crate::{MarketplaceRoyalties, SorobanForgeMarketplaceRoyaltiesClient};
use soroban_sdk::testutils::{Address as _, MockAuth, MockAuthInvoke};
use soroban_sdk::token::StellarAssetClient;
use soroban_sdk::{Address, Env, IntoVal, InvokeError};

const BPS: u32 = 500;
const AMOUNT: i128 = 1_000;

macro_rules! setup {
    () => {{
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let sac = env.register_stellar_asset_contract_v2(admin);
        let token = sac.address();
        let token_admin = StellarAssetClient::new(&env, &token);

        let collection = Address::generate(&env);
        let recipient = Address::generate(&env);
        let seller = Address::generate(&env);
        let payer = Address::generate(&env);

        token_admin.mint(&payer, &10_000_i128);

        let contract_id = env.register(MarketplaceRoyalties, ());
        let client = SorobanForgeMarketplaceRoyaltiesClient::new(&env, &contract_id);

        (
            env,
            token,
            contract_id,
            client,
            collection,
            recipient,
            seller,
            payer,
        )
    }};
}

macro_rules! assert_auth_abort {
    ($res:expr) => {
        assert!(
            matches!($res, Err(Err(InvokeError::Abort))),
            "expected auth abort, got {:?}",
            $res
        );
    };
}

#[test]
fn set_royalty_accepts_collection_signature() {
    let (env, _token, contract_id, client, collection, recipient, _seller, _payer) = setup!();

    env.mock_auths(&[MockAuth {
        address: &collection,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "set_royalty",
            args: (&collection, &recipient, BPS, false).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    client
        .try_set_royalty(&collection, &recipient, &BPS, &false)
        .expect("outer ok")
        .expect("contract ok");

    let royalty = client.get_royalty(&collection);
    assert_eq!(royalty.recipient, recipient);
    assert_eq!(royalty.bps, BPS);
}

#[test]
fn set_royalty_rejects_signature_from_non_collection() {
    let (env, _token, contract_id, client, collection, recipient, _seller, _payer) = setup!();

    // Recipient trying to set royalty instead of collection
    env.mock_auths(&[MockAuth {
        address: &recipient,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "set_royalty",
            args: (&collection, &recipient, BPS, false).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_set_royalty(&collection, &recipient, &BPS, &false);
    assert_auth_abort!(res);
}

#[test]
fn distribute_accepts_collection_and_payer_signatures() {
    let (env, token, contract_id, client, collection, recipient, seller, payer) = setup!();

    client.set_royalty(&collection, &recipient, &BPS, &false);

    // Two frames: the collection authorizes the entrypoint, and the payer
    // authorizes the entrypoint — its frame also carries the nested royalty
    // transfer to the recipient (50 = 5% of 1_000) as a sub-invocation,
    // exactly as escrow's deposit carries its nested SAC transfer.
    env.mock_auths(&[
        MockAuth {
            address: &collection,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "distribute",
                args: (&collection, &token, &payer, &seller, AMOUNT).into_val(&env),
                sub_invokes: &[],
            },
        },
        MockAuth {
            address: &payer,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "distribute",
                args: (&collection, &token, &payer, &seller, AMOUNT).into_val(&env),
                sub_invokes: &[MockAuthInvoke {
                    contract: &token,
                    fn_name: "transfer",
                    args: (&payer, &recipient, 50_i128).into_val(&env),
                    sub_invokes: &[],
                }],
            },
        },
    ]);

    let net = client
        .try_distribute(&collection, &token, &payer, &seller, &AMOUNT)
        .expect("outer ok")
        .unwrap();
    assert_eq!(net, 950);
}

#[test]
fn distribute_rejects_seller_signature() {
    let (env, token, contract_id, client, collection, recipient, seller, payer) = setup!();

    client.set_royalty(&collection, &recipient, &BPS, &false);

    // Seller trying to authorize distribute instead of collection. The
    // payer's entrypoint frame still carries the nested token transfer, so
    // only the seller's wrongful collection-frame triggers the abort.
    env.mock_auths(&[
        MockAuth {
            address: &seller,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "distribute",
                args: (&collection, &token, &payer, &seller, AMOUNT).into_val(&env),
                sub_invokes: &[],
            },
        },
        MockAuth {
            address: &payer,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "distribute",
                args: (&collection, &token, &payer, &seller, AMOUNT).into_val(&env),
                sub_invokes: &[MockAuthInvoke {
                    contract: &token,
                    fn_name: "transfer",
                    args: (&payer, &recipient, 50_i128).into_val(&env),
                    sub_invokes: &[],
                }],
            },
        },
    ]);

    let res = client.try_distribute(&collection, &token, &payer, &seller, &AMOUNT);
    assert_auth_abort!(res);
}

#[test]
fn distribute_rejects_payer_signature_without_token_authorization() {
    let (env, token, contract_id, client, collection, recipient, seller, payer) = setup!();

    client.set_royalty(&collection, &recipient, &BPS, &false);

    // Only the entrypoint frames are armed; the nested SAC transfer pull
    // has no authorization. Funds must not move on entrypoint signatures
    // alone.
    env.mock_auths(&[
        MockAuth {
            address: &collection,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "distribute",
                args: (&collection, &token, &payer, &seller, AMOUNT).into_val(&env),
                sub_invokes: &[],
            },
        },
        MockAuth {
            address: &payer,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "distribute",
                args: (&collection, &token, &payer, &seller, AMOUNT).into_val(&env),
                sub_invokes: &[],
            },
        },
    ]);

    // The unmatched nested auth is not a root abort: the SAC rejects the
    // pull and the contract buckets the token error.
    let res = client.try_distribute(&collection, &token, &payer, &seller, &AMOUNT);
    assert!(matches!(
        res,
        Err(Ok(
            soroban_forge_shared_utils::ForgeError::TokenTransferFailed
        ))
    ));
}

#[test]
fn settle_sale_accepts_collection_and_payer_signatures_and_records_auth_tree() {
    use soroban_sdk::token::StellarAssetClient as TokenAdminClient;

    let (env, token, contract_id, client, collection, recipient, seller, payer) = setup!();
    client.set_royalty(&collection, &recipient, &BPS, &false);

    env.mock_all_auths();
    let token_admin = TokenAdminClient::new(&env, &token);
    token_admin.mint(&payer, &(AMOUNT * 10));

    let expected_royalty = AMOUNT * (BPS as i128) / 10_000;
    let expected_seller_net = AMOUNT - expected_royalty;

    let transfer_sub_invokes = [
        MockAuthInvoke {
            contract: &token,
            fn_name: "transfer",
            args: (&payer, &seller, expected_seller_net).into_val(&env),
            sub_invokes: &[],
        },
        MockAuthInvoke {
            contract: &token,
            fn_name: "transfer",
            args: (&payer, &recipient, expected_royalty).into_val(&env),
            sub_invokes: &[],
        },
    ];

    env.mock_auths(&[
        MockAuth {
            address: &collection,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "settle_sale",
                args: (&collection, &token, &payer, &seller, AMOUNT).into_val(&env),
                sub_invokes: &[],
            },
        },
        MockAuth {
            address: &payer,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "settle_sale",
                args: (&collection, &token, &payer, &seller, AMOUNT).into_val(&env),
                sub_invokes: &transfer_sub_invokes,
            },
        },
    ]);

    let res = client
        .try_settle_sale(&collection, &token, &payer, &seller, &AMOUNT)
        .expect("outer ok")
        .expect("contract ok");

    assert_eq!(res.seller_net + res.royalty_share, AMOUNT);
    assert!(!env.auths().is_empty(), "expected auth tree records");
}

#[test]
fn settle_sale_rejects_missing_collection_signature() {
    let (env, token, contract_id, client, collection, recipient, seller, payer) = setup!();
    client.set_royalty(&collection, &recipient, &BPS, &false);

    env.mock_auths(&[MockAuth {
        address: &payer,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "settle_sale",
            args: (&collection, &token, &payer, &seller, AMOUNT).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_settle_sale(&collection, &token, &payer, &seller, &AMOUNT);
    assert_auth_abort!(res);
}

#[test]
fn settle_sale_rejects_missing_payer_signature() {
    let (env, token, contract_id, client, collection, recipient, seller, payer) = setup!();
    client.set_royalty(&collection, &recipient, &BPS, &false);

    env.mock_auths(&[MockAuth {
        address: &collection,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "settle_sale",
            args: (&collection, &token, &payer, &seller, AMOUNT).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_settle_sale(&collection, &token, &payer, &seller, &AMOUNT);
    assert_auth_abort!(res);
}

#[test]
fn settle_sale_rejects_non_party_signature_in_place_of_payer() {
    let (env, token, contract_id, client, collection, recipient, seller, payer) = setup!();
    client.set_royalty(&collection, &recipient, &BPS, &false);

    env.mock_auths(&[
        MockAuth {
            address: &collection,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "settle_sale",
                args: (&collection, &token, &payer, &seller, AMOUNT).into_val(&env),
                sub_invokes: &[],
            },
        },
        MockAuth {
            address: &seller,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "settle_sale",
                args: (&collection, &token, &payer, &seller, AMOUNT).into_val(&env),
                sub_invokes: &[],
            },
        },
    ]);

    let res = client.try_settle_sale(&collection, &token, &payer, &seller, &AMOUNT);
    assert_auth_abort!(res);
}

#[test]
fn settle_sale_rejects_replayed_signature_with_altered_args() {
    let (env, token, contract_id, client, collection, recipient, seller, payer) = setup!();
    client.set_royalty(&collection, &recipient, &BPS, &false);

    let altered_amount = AMOUNT * 2;

    env.mock_auths(&[
        MockAuth {
            address: &collection,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "settle_sale",
                args: (&collection, &token, &payer, &seller, AMOUNT).into_val(&env),
                sub_invokes: &[],
            },
        },
        MockAuth {
            address: &payer,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "settle_sale",
                args: (&collection, &token, &payer, &seller, AMOUNT).into_val(&env),
                sub_invokes: &[],
            },
        },
    ]);

    let res = client.try_settle_sale(&collection, &token, &payer, &seller, &altered_amount);
    assert_auth_abort!(res);
}

#[test]
fn settle_sales_accepts_valid_batch_and_records_auth_tree() {
    use soroban_sdk::token::StellarAssetClient as TokenAdminClient;
    use soroban_sdk::vec;

    let (env, token, contract_id, client, collection, recipient, seller, payer) = setup!();
    client.set_royalty(&collection, &recipient, &BPS, &false);

    env.mock_all_auths();
    let token_admin = TokenAdminClient::new(&env, &token);
    token_admin.mint(&payer, &(AMOUNT * 10));

    let expected_royalty = AMOUNT * (BPS as i128) / 10_000;
    let expected_seller_net = AMOUNT - expected_royalty;

    let sales = vec![&env, (seller.clone(), AMOUNT), (seller.clone(), AMOUNT)];

    let batch_sub_invokes = [
        MockAuthInvoke {
            contract: &token,
            fn_name: "transfer",
            args: (&payer, &seller, expected_seller_net).into_val(&env),
            sub_invokes: &[],
        },
        MockAuthInvoke {
            contract: &token,
            fn_name: "transfer",
            args: (&payer, &recipient, expected_royalty).into_val(&env),
            sub_invokes: &[],
        },
        MockAuthInvoke {
            contract: &token,
            fn_name: "transfer",
            args: (&payer, &seller, expected_seller_net).into_val(&env),
            sub_invokes: &[],
        },
        MockAuthInvoke {
            contract: &token,
            fn_name: "transfer",
            args: (&payer, &recipient, expected_royalty).into_val(&env),
            sub_invokes: &[],
        },
    ];

    env.mock_auths(&[
        MockAuth {
            address: &collection,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "settle_sales",
                args: (&collection, &token, &payer, sales.clone()).into_val(&env),
                sub_invokes: &[],
            },
        },
        MockAuth {
            address: &payer,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "settle_sales",
                args: (&collection, &token, &payer, sales.clone()).into_val(&env),
                sub_invokes: &batch_sub_invokes,
            },
        },
    ]);

    let settlements = client
        .try_settle_sales(&collection, &token, &payer, &sales)
        .expect("outer ok")
        .expect("contract ok");

    assert_eq!(settlements.len(), 2);
    assert!(!env.auths().is_empty());
}

#[test]
fn settle_sales_auth_failure_leaves_summary_and_balances_untouched() {
    use soroban_sdk::token::Client as TokenClient;
    use soroban_sdk::vec;

    let (env, token, contract_id, client, collection, recipient, seller, payer) = setup!();
    client.set_royalty(&collection, &recipient, &BPS, &false);

    let token_client = TokenClient::new(&env, &token);
    let initial_payer_balance = token_client.balance(&payer);
    let initial_summary = client.try_get_settlement_summary(&collection);

    let sales = vec![&env, (seller.clone(), AMOUNT), (seller.clone(), AMOUNT)];

    env.mock_auths(&[MockAuth {
        address: &payer,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "settle_sales",
            args: (&collection, &token, &payer, sales.clone()).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_settle_sales(&collection, &token, &payer, &sales);
    assert_auth_abort!(res);

    assert_eq!(token_client.balance(&payer), initial_payer_balance);
    assert_eq!(
        client.try_get_settlement_summary(&collection),
        initial_summary
    );
}

#[test]
fn distribute_accrued_accepts_collection_signature() {
    use soroban_sdk::token::StellarAssetClient as TokenAdminClient;

    let (env, token, contract_id, client, collection, recipient, seller, payer) = setup!();
    client.set_royalty(&collection, &recipient, &BPS, &true);

    env.mock_all_auths();
    let token_admin = TokenAdminClient::new(&env, &token);
    token_admin.mint(&payer, &(AMOUNT * 10));

    client.settle_sale(&collection, &token, &payer, &seller, &AMOUNT);
    let expected_accrual = AMOUNT * (BPS as i128) / 10_000;
    assert_eq!(client.get_accrued(&collection, &token), expected_accrual);

    env.mock_auths(&[MockAuth {
        address: &collection,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "distribute_accrued",
            args: (&collection, &token).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let swept = client
        .try_distribute_accrued(&collection, &token)
        .expect("outer ok")
        .expect("contract ok");
    assert_eq!(swept, expected_accrual);
}

#[test]
fn distribute_accrued_rejects_non_collection_signature() {
    use soroban_sdk::token::StellarAssetClient as TokenAdminClient;

    let (env, token, contract_id, client, collection, recipient, seller, payer) = setup!();
    client.set_royalty(&collection, &recipient, &BPS, &true);

    env.mock_all_auths();
    let token_admin = TokenAdminClient::new(&env, &token);
    token_admin.mint(&payer, &(AMOUNT * 10));
    client.settle_sale(&collection, &token, &payer, &seller, &AMOUNT);

    // The recipient trying to authorize the sweep instead of the collection.
    env.mock_auths(&[MockAuth {
        address: &recipient,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "distribute_accrued",
            args: (&collection, &token).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_distribute_accrued(&collection, &token);
    assert_auth_abort!(res);
}
