//! Negative authorization tests for the wallet's state-changing entrypoints.
//!
//! The main suite (`tests` in lib.rs) runs under `mock_all_auths`, which
//! proves the *call graph* of authorizations — who the contract asks to
//! sign — but never that a wrong signer is rejected. This module covers the
//! other half for every state-changing entrypoint (`initialize`, `submit`,
//! `confirm`, `execute`), in two layers:
//!
//! 1. **Enforce mode** (`mock_auths` / `set_auths`): each test arms
//!    authorization for exactly the signer a scenario names and asserts
//!    that anyone else — or the right signer over the wrong arguments — is
//!    rejected by the host, with wallet state untouched.
//! 2. **Tree assertions** (`env.auths()` under `mock_all_auths`): pins the
//!    exact authorized-invocation tree each path demands, per the SDK's own
//!    recommendation for tests that would otherwise prove nothing about
//!    missing `require_auth` calls.
//!
//! The mechanics (mirroring `crates/escrow/src/authz.rs` and
//! `crates/dao-governance/src/authz.rs`, worth re-reading for the long
//! form):
//!
//! - `mock_auths` sets the invocation envelope **and disables blanket
//!   mocking**; authorizations not matching a mocked auth fail.
//! - A missing/extra/mismatched auth aborts the whole invocation
//!   (`Err(Err(InvokeError::Abort))` through the `try_` client).
//! - `set_auths(&[])` is a blank envelope: every `require_auth` fails.
//! - `initialize` performs **no** `require_auth` — the deployer-initializer
//!   pattern trusts the deployment transaction itself — so its negative
//!   coverage below proves the *validation* guards instead (single init,
//!   threshold bounds, duplicate owners), which are the only rejection
//!   paths that entrypoint has.
//!
//! **Soroban host limitation note.** Under the test host the *absence* of a
//! `require_auth` cannot be distinguished from a satisfied one while
//! `mock_all_auths` is armed — the host auto-approves. That is exactly what
//! the tree assertions catch: if the `require_auth(signer)` call were
//! removed from `submit` or `confirm`, the recorded tree would shrink to
//! nothing and `submit_authorization_tree_...` /
//! `confirm_authorization_tree_...` would fail. The mutation drill in the
//! PR description removes that call and observes precisely those failures.
//!
//! Wallet entrypoints carry no token pulls inside the tested paths, so no
//! nested sub-invocations appear in any tree (unlike the DAO's bond pull).

use crate::{
    LimitChange, MultiSigWallet, SorobanForgeMultiSigWalletClient, TxKind, TxStatus,
    WithdrawalLimit,
};
use soroban_forge_test_utils::{MockTarget, TestAccounts};
// The test harness links std even in a no_std crate; AuthorizedInvocation's
// sub_invocations field is a std Vec, so re-expose std here for `vec!`.
extern crate std;
use soroban_sdk::testutils::{
    Address as _, AuthorizedFunction, AuthorizedInvocation, MockAuth, MockAuthInvoke,
};
use soroban_sdk::token::StellarAssetClient;
use soroban_sdk::{Address, Bytes, Env, IntoVal, InvokeError, Symbol};

const DEPOSIT_AMOUNT: i128 = 1_000;
const LIMIT_AMOUNT: i128 = 2_000;
const WINDOW_SECONDS: u64 = 3_600;

/// Fresh env with blanket mocking for *setup only*. The tested call re-arms
/// the envelope afterwards.
///
/// Returns `(env, contract_id, client, accounts)`.
macro_rules! setup {
    () => {{
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(MultiSigWallet, ());
        let client = SorobanForgeMultiSigWalletClient::new(&env, &contract_id);
        let accounts = TestAccounts::generate(&env);
        (env, contract_id, client, accounts)
    }};
}

fn payload(env: &Env) -> Bytes {
    Bytes::from_array(env, &[0x01, 0x02, 0x03])
}

/// The standard 3-owner set (user1, user2, user3) at threshold 2.
fn owner_vec(env: &Env, accounts: &TestAccounts) -> soroban_sdk::Vec<soroban_sdk::Address> {
    soroban_sdk::vec![
        env,
        accounts.user1.clone(),
        accounts.user2.clone(),
        accounts.user3.clone()
    ]
}

fn initialize_wallet(
    env: &Env,
    client: &SorobanForgeMultiSigWalletClient<'_>,
    accounts: &TestAccounts,
) {
    client.initialize(&owner_vec(env, accounts), &2_u32);
}

fn funded_token(env: &Env, account: &Address, amount: i128) -> Address {
    let admin = Address::generate(env);
    let token = env.register_stellar_asset_contract_v2(admin).address();
    StellarAssetClient::new(env, &token).mint(account, &amount);
    token
}

fn install_withdrawal_limit(
    client: &SorobanForgeMultiSigWalletClient<'_>,
    accounts: &TestAccounts,
    token: &Address,
) -> u64 {
    let tx_id = client.set_withdrawal_limit(&accounts.user1, token, &LIMIT_AMOUNT, &WINDOW_SECONDS);
    client.confirm(&tx_id, &accounts.user2);
    client.confirm(&tx_id, &accounts.user3);
    client.execute(&tx_id);
    tx_id
}

/// Assert that a `try_` call aborted on authorization (the host aborts the
/// whole invocation when the armed envelope does not match).
macro_rules! assert_auth_abort {
    ($res:expr) => {
        assert!(
            matches!($res, Err(Err(InvokeError::Abort))),
            "expected auth abort, got {:?}",
            $res
        );
    };
}

// -----------------------------------------------------------------------
// initialize — deployer-trusted, so negative coverage proves the
// validation guards (the only rejection paths it has)
// -----------------------------------------------------------------------

#[test]
fn initialize_rejects_a_second_initialization() {
    let (_env, _contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&_env, &accounts), &2_u32);

    // Re-initialization must be rejected: the wallet's owner set and
    // threshold must never be replaceable after deployment.
    let err = client
        .try_initialize(&owner_vec(&_env, &accounts), &1_u32)
        .unwrap_err()
        .unwrap();
    assert_eq!(
        err,
        soroban_forge_shared_utils::ForgeError::AlreadyInitialized
    );
    assert_eq!(client.try_get_threshold().unwrap().unwrap(), 2_u32);
}

#[test]
fn initialize_rejects_threshold_above_owner_count() {
    let (_env, _contract_id, client, accounts) = setup!();

    // A threshold above the owner count would make the wallet permanently
    // unable to execute — rejected as InvalidInput.
    let err = client
        .try_initialize(&owner_vec(&_env, &accounts), &4_u32)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, soroban_forge_shared_utils::ForgeError::InvalidInput);
}

#[test]
fn initialize_rejects_duplicate_owners() {
    let (env, _contract_id, client, accounts) = setup!();

    // A duplicated owner address would let one party cast two
    // confirmations toward the threshold.
    let dup = soroban_sdk::vec![
        &env,
        accounts.user1.clone(),
        accounts.user1.clone(),
        accounts.user2.clone()
    ];
    let err = client.try_initialize(&dup, &2_u32).unwrap_err().unwrap();
    assert_eq!(err, soroban_forge_shared_utils::ForgeError::InvalidInput);
}

// -----------------------------------------------------------------------
// submit — owner-only
// -----------------------------------------------------------------------

#[test]
fn submit_accepts_the_owner_signature() {
    let (env, contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());

    env.mock_auths(&[MockAuth {
        address: &accounts.user1,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "submit",
            args: (&accounts.user1, &target, payload(&env), None::<u64>).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let tx_id = client
        .try_submit(&accounts.user1, &target, &payload(&env), &None)
        .expect("outer ok")
        .expect("contract ok");
    assert_eq!(tx_id, 1);
    assert_eq!(client.try_get_tx_count().unwrap().unwrap(), 1_u64);
}

#[test]
fn submit_rejects_signature_from_a_non_owner() {
    let (env, contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());

    // A stranger (deployer is not an owner) arms their own signature for a
    // submit whose `submitter` argument claims to be user1. The contract
    // demands `submitter.require_auth()`, so the host must reject.
    env.mock_auths(&[MockAuth {
        address: &accounts.deployer,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "submit",
            args: (&accounts.user1, &target, payload(&env), None::<u64>).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_submit(&accounts.user1, &target, &payload(&env), &None);
    assert_auth_abort!(res);
    assert_eq!(client.try_get_tx_count().unwrap().unwrap(), 0_u64);
}

#[test]
fn submit_without_authorization_aborts_and_writes_nothing() {
    let (env, _contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);

    // Blank envelope: every require_auth fails.
    env.set_auths(&[]);

    let target = env.register(MockTarget, ());
    let res = client.try_submit(&accounts.user1, &target, &payload(&env), &None);
    assert_auth_abort!(res);
    assert_eq!(client.try_get_tx_count().unwrap().unwrap(), 0_u64);
}

#[test]
fn submit_authorization_tree_is_the_owner_entrypoint_frame() {
    let (env, contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());

    client.submit(&accounts.user1, &target, &payload(&env), &None);

    // The tree is the submitter's entrypoint frame only — a removed
    // `require_auth(submitter)` shrinks this to empty and fails the test.
    assert_eq!(
        env.auths(),
        [(
            accounts.user1.clone(),
            AuthorizedInvocation {
                function: AuthorizedFunction::Contract((
                    contract_id.clone(),
                    Symbol::new(&env, "submit"),
                    (
                        accounts.user1.clone(),
                        target.clone(),
                        payload(&env),
                        None::<u64>
                    )
                        .into_val(&env),
                )),
                sub_invocations: std::vec![],
            },
        )],
    );
}

#[test]
fn submit_by_a_non_owner_is_unauthorized_even_under_mocked_auths() {
    let (env, _contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());

    // Under blanket mocking the identity check is what rejects: the
    // non-owner deployer gets the typed error, not a host abort.
    let err = client
        .try_submit(&accounts.deployer, &target, &payload(&env), &None)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, soroban_forge_shared_utils::ForgeError::Unauthorized);
}

// -----------------------------------------------------------------------
// confirm — owner-only, once, pending-only
// -----------------------------------------------------------------------

#[test]
fn confirm_accepts_the_owner_signature() {
    let (env, contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());
    let tx_id = client.submit(&accounts.user1, &target, &payload(&env), &None);

    env.mock_auths(&[MockAuth {
        address: &accounts.user2,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "confirm",
            args: (tx_id, &accounts.user2).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    client
        .try_confirm(&tx_id, &accounts.user2)
        .expect("outer ok")
        .expect("contract ok");

    let tx = client
        .try_get_tx(&tx_id)
        .expect("outer ok")
        .expect("contract ok");
    assert_eq!(tx.confirmations.len(), 1_u32);
}

#[test]
fn confirm_rejects_signature_from_a_non_owner() {
    let (env, contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());
    let tx_id = client.submit(&accounts.user1, &target, &payload(&env), &None);

    // A stranger (deployer) signs a confirm whose `signer` argument names
    // user2. The contract calls `signer.require_auth()`, so the mismatch
    // aborts the invocation.
    env.mock_auths(&[MockAuth {
        address: &accounts.deployer,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "confirm",
            args: (tx_id, &accounts.user2).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_confirm(&tx_id, &accounts.user2);
    assert_auth_abort!(res);

    let tx = client
        .try_get_tx(&tx_id)
        .expect("outer ok")
        .expect("contract ok");
    assert_eq!(tx.confirmations.len(), 0_u32);
}

#[test]
fn confirm_rejects_signature_over_a_different_signer_argument() {
    let (env, contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());
    let tx_id = client.submit(&accounts.user1, &target, &payload(&env), &None);

    // user2 arms a signature for *their own* confirm, but the invocation
    // runs with user3 as the signer — a captured signature must not be
    // replayable for a different owner.
    env.mock_auths(&[MockAuth {
        address: &accounts.user2,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "confirm",
            args: (tx_id, &accounts.user2).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let res = client.try_confirm(&tx_id, &accounts.user3);
    assert_auth_abort!(res);

    let tx = client
        .try_get_tx(&tx_id)
        .expect("outer ok")
        .expect("contract ok");
    assert_eq!(tx.confirmations.len(), 0_u32);
}

#[test]
fn confirm_authorization_tree_is_the_owner_entrypoint_frame() {
    let (env, contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());
    let tx_id = client.submit(&accounts.user1, &target, &payload(&env), &None);

    client.confirm(&tx_id, &accounts.user2);

    // The tree is the confirming owner's frame only — a removed
    // `require_auth(signer)` shrinks this to empty and fails the test.
    assert_eq!(
        env.auths(),
        [(
            accounts.user2.clone(),
            AuthorizedInvocation {
                function: AuthorizedFunction::Contract((
                    contract_id.clone(),
                    Symbol::new(&env, "confirm"),
                    (tx_id, accounts.user2.clone()).into_val(&env),
                )),
                sub_invocations: std::vec![],
            },
        )],
    );
}

#[test]
fn blank_envelope_aborts_confirm_and_preserves_state() {
    let (env, _contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());
    let tx_id = client.submit(&accounts.user1, &target, &payload(&env), &None);

    env.set_auths(&[]);
    let res = client.try_confirm(&tx_id, &accounts.user2);
    assert_auth_abort!(res);

    // The record must be untouched.
    let tx = client
        .try_get_tx(&tx_id)
        .expect("outer ok")
        .expect("contract ok");
    assert_eq!(tx.confirmations.len(), 0_u32);
    assert_eq!(tx.status, crate::TxStatus::Pending);
}

// -----------------------------------------------------------------------
// execute — permissionless once the threshold is met
// -----------------------------------------------------------------------

#[test]
fn execute_is_permissionless_at_threshold_under_a_blank_envelope() {
    let (env, _contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());
    let tx_id = client.submit(&accounts.user1, &target, &payload(&env), &None);
    client.confirm(&tx_id, &accounts.user1);
    client.confirm(&tx_id, &accounts.user2);

    // Execute demands no signature: with the threshold met a blank
    // envelope still completes the state transition.
    env.set_auths(&[]);
    client.execute(&tx_id);

    let tx = client
        .try_get_tx(&tx_id)
        .expect("outer ok")
        .expect("contract ok");
    assert_eq!(tx.status, crate::TxStatus::Executed);
    assert_eq!(env.auths().len(), 0);
}

#[test]
fn execute_below_threshold_is_invalid_input_even_for_owners() {
    let (env, _contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());
    let tx_id = client.submit(&accounts.user1, &target, &payload(&env), &None);
    client.confirm(&tx_id, &accounts.user1);

    // A single confirmation below the threshold of 2 must never execute —
    // this is the invariant props.rs P1 also covers, pinned deterministically.
    let err = client.try_execute(&tx_id).unwrap_err().unwrap();
    assert_eq!(err, soroban_forge_shared_utils::ForgeError::InvalidInput);
    let tx = client
        .try_get_tx(&tx_id)
        .expect("outer ok")
        .expect("contract ok");
    assert_eq!(tx.status, crate::TxStatus::Pending);
}

#[test]
fn execute_of_an_already_executed_tx_is_rejected() {
    let (env, _contract_id, client, accounts) = setup!();
    client.initialize(&owner_vec(&env, &accounts), &2_u32);
    let target = env.register(MockTarget, ());
    let tx_id = client.submit(&accounts.user1, &target, &payload(&env), &None);
    client.confirm(&tx_id, &accounts.user1);
    client.confirm(&tx_id, &accounts.user2);
    client.execute(&tx_id);

    // Terminal state: replay must be rejected (the once-only invariant,
    // pinned deterministically; props.rs P2 covers it over random inputs).
    let err = client.try_execute(&tx_id).unwrap_err().unwrap();
    assert_eq!(err, soroban_forge_shared_utils::ForgeError::InvalidInput);
}

// -----------------------------------------------------------------------
// reject — owner-only veto
// -----------------------------------------------------------------------

#[test]
fn reject_accepts_owner_signature_below_threshold() {
    let (env, contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let target = env.register(MockTarget, ());
    let tx_id = client.submit(&accounts.user1, &target, &payload(&env), &None);

    env.mock_auths(&[MockAuth {
        address: &accounts.user2,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "reject",
            args: (tx_id, &accounts.user2).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    client
        .try_reject(&tx_id, &accounts.user2)
        .expect("outer ok")
        .expect("contract ok");
    let tx = client.get_tx(&tx_id);
    assert_eq!(tx.rejections.len(), 1);
    assert_eq!(tx.status, TxStatus::Pending);
}

#[test]
fn reject_returns_unauthorized_for_non_owner() {
    let (env, _contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let target = env.register(MockTarget, ());
    let tx_id = client.submit(&accounts.user1, &target, &payload(&env), &None);

    let err = client
        .try_reject(&tx_id, &accounts.deployer)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, soroban_forge_shared_utils::ForgeError::Unauthorized);
    let tx = client.get_tx(&tx_id);
    assert_eq!(tx.rejections.len(), 0);
    assert_eq!(tx.status, TxStatus::Pending);
}

#[test]
fn reject_rejects_signature_replayed_for_another_tx() {
    let (env, contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let target = env.register(MockTarget, ());
    let first_tx = client.submit(&accounts.user1, &target, &payload(&env), &None);
    let second_tx = client.submit(&accounts.user1, &target, &payload(&env), &None);

    env.mock_auths(&[MockAuth {
        address: &accounts.user2,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "reject",
            args: (first_tx, &accounts.user2).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    assert_auth_abort!(client.try_reject(&second_tx, &accounts.user2));
    assert_eq!(client.get_tx(&first_tx).rejections.len(), 0);
    assert_eq!(client.get_tx(&second_tx).rejections.len(), 0);
}

#[test]
fn reject_twice_by_same_owner_is_invalid() {
    let (env, _contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let target = env.register(MockTarget, ());
    let tx_id = client.submit(&accounts.user1, &target, &payload(&env), &None);
    client.reject(&tx_id, &accounts.user2);

    let err = client
        .try_reject(&tx_id, &accounts.user2)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, soroban_forge_shared_utils::ForgeError::InvalidInput);
    assert_eq!(client.get_tx(&tx_id).rejections.len(), 1);
}

#[test]
fn blank_envelope_aborts_reject_without_changing_tx() {
    let (env, _contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let target = env.register(MockTarget, ());
    let tx_id = client.submit(&accounts.user1, &target, &payload(&env), &None);
    env.set_auths(&[]);

    assert_auth_abort!(client.try_reject(&tx_id, &accounts.user2));
    let tx = client.get_tx(&tx_id);
    assert_eq!(tx.rejections.len(), 0);
    assert_eq!(tx.status, TxStatus::Pending);
}

// -----------------------------------------------------------------------
// deposit — depositor-authorized custody pull
// -----------------------------------------------------------------------

#[test]
fn deposit_accepts_from_signature_and_nested_token_authorization() {
    let (env, contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = funded_token(&env, &accounts.user1, DEPOSIT_AMOUNT);

    let transfer_sub_invokes = [MockAuthInvoke {
        contract: &token,
        fn_name: "transfer",
        args: (&accounts.user1, &contract_id, DEPOSIT_AMOUNT).into_val(&env),
        sub_invokes: &[],
    }];
    env.mock_auths(&[MockAuth {
        address: &accounts.user1,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "deposit",
            args: (&token, &accounts.user1, DEPOSIT_AMOUNT).into_val(&env),
            sub_invokes: &transfer_sub_invokes,
        },
    }]);

    client
        .try_deposit(&token, &accounts.user1, &DEPOSIT_AMOUNT)
        .expect("outer ok")
        .expect("contract ok");
    assert_eq!(client.balance(&token), DEPOSIT_AMOUNT);
    assert_eq!(
        StellarAssetClient::new(&env, &token).balance(&accounts.user1),
        0
    );
    assert_eq!(
        StellarAssetClient::new(&env, &token).balance(&contract_id),
        DEPOSIT_AMOUNT
    );
}

#[test]
fn deposit_rejects_signature_from_a_different_party() {
    let (env, contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = funded_token(&env, &accounts.user1, DEPOSIT_AMOUNT);

    env.mock_auths(&[MockAuth {
        address: &accounts.deployer,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "deposit",
            args: (&token, &accounts.user1, DEPOSIT_AMOUNT).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    assert_auth_abort!(client.try_deposit(&token, &accounts.user1, &DEPOSIT_AMOUNT));
    assert_eq!(client.balance(&token), 0);
    assert_eq!(
        StellarAssetClient::new(&env, &token).balance(&accounts.user1),
        DEPOSIT_AMOUNT
    );
    assert_eq!(
        StellarAssetClient::new(&env, &token).balance(&contract_id),
        0
    );
}

#[test]
fn blank_envelope_aborts_deposit_without_changing_custody_or_balances() {
    let (env, contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = funded_token(&env, &accounts.user1, DEPOSIT_AMOUNT);
    env.set_auths(&[]);

    assert_auth_abort!(client.try_deposit(&token, &accounts.user1, &DEPOSIT_AMOUNT));
    assert_eq!(client.balance(&token), 0);
    assert_eq!(
        StellarAssetClient::new(&env, &token).balance(&accounts.user1),
        DEPOSIT_AMOUNT
    );
    assert_eq!(
        StellarAssetClient::new(&env, &token).balance(&contract_id),
        0
    );
}

#[test]
fn deposit_authorization_tree_is_depositor_over_token_transfer() {
    let (env, contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = funded_token(&env, &accounts.user1, DEPOSIT_AMOUNT);

    client.deposit(&token, &accounts.user1, &DEPOSIT_AMOUNT);

    assert_eq!(
        env.auths(),
        [(
            accounts.user1.clone(),
            AuthorizedInvocation {
                function: AuthorizedFunction::Contract((
                    contract_id.clone(),
                    Symbol::new(&env, "deposit"),
                    (&token, &accounts.user1, DEPOSIT_AMOUNT).into_val(&env),
                )),
                sub_invocations: std::vec![AuthorizedInvocation {
                    function: AuthorizedFunction::Contract((
                        token.clone(),
                        Symbol::new(&env, "transfer"),
                        (&accounts.user1, &contract_id, DEPOSIT_AMOUNT).into_val(&env),
                    )),
                    sub_invocations: std::vec![],
                }],
            },
        )],
    );
}

// -----------------------------------------------------------------------
// submit_withdrawal — owner-only and reserves window usage
// -----------------------------------------------------------------------

#[test]
fn submit_withdrawal_accepts_owner_signature_and_records_usage() {
    let (env, contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = funded_token(&env, &accounts.user1, DEPOSIT_AMOUNT);
    client.deposit(&token, &accounts.user1, &DEPOSIT_AMOUNT);
    install_withdrawal_limit(&client, &accounts, &token);

    env.mock_auths(&[MockAuth {
        address: &accounts.user1,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "submit_withdrawal",
            args: (
                &accounts.user1,
                &token,
                &accounts.arbiter,
                DEPOSIT_AMOUNT / 2,
            )
                .into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let tx_id = client
        .try_submit_withdrawal(
            &accounts.user1,
            &token,
            &accounts.arbiter,
            &(DEPOSIT_AMOUNT / 2),
        )
        .expect("outer ok")
        .expect("contract ok");
    assert_eq!(client.get_tx(&tx_id).submitter, accounts.user1);
    assert_eq!(client.get_tx_count(), 2);
    assert_eq!(client.get_window_usage(&token), DEPOSIT_AMOUNT / 2);
}

#[test]
fn submit_withdrawal_rejects_wrong_owner_signature() {
    let (env, contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = funded_token(&env, &accounts.user1, DEPOSIT_AMOUNT);
    client.deposit(&token, &accounts.user1, &DEPOSIT_AMOUNT);
    install_withdrawal_limit(&client, &accounts, &token);
    let count_before = client.get_tx_count();

    env.mock_auths(&[MockAuth {
        address: &accounts.deployer,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "submit_withdrawal",
            args: (
                &accounts.user1,
                &token,
                &accounts.arbiter,
                DEPOSIT_AMOUNT / 2,
            )
                .into_val(&env),
            sub_invokes: &[],
        },
    }]);

    assert_auth_abort!(client.try_submit_withdrawal(
        &accounts.user1,
        &token,
        &accounts.arbiter,
        &(DEPOSIT_AMOUNT / 2)
    ));
    assert_eq!(client.get_tx_count(), count_before);
    assert_eq!(client.get_window_usage(&token), 0);
    assert_eq!(client.balance(&token), DEPOSIT_AMOUNT);
}

#[test]
fn submit_withdrawal_rejects_signature_replayed_with_different_amount() {
    let (env, contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = funded_token(&env, &accounts.user1, DEPOSIT_AMOUNT);
    client.deposit(&token, &accounts.user1, &DEPOSIT_AMOUNT);
    install_withdrawal_limit(&client, &accounts, &token);
    let count_before = client.get_tx_count();

    env.mock_auths(&[MockAuth {
        address: &accounts.user1,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "submit_withdrawal",
            args: (&accounts.user1, &token, &accounts.arbiter, 400_i128).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    assert_auth_abort!(client.try_submit_withdrawal(
        &accounts.user1,
        &token,
        &accounts.arbiter,
        &500_i128
    ));
    assert_eq!(client.get_tx_count(), count_before);
    assert_eq!(client.get_window_usage(&token), 0);
}

#[test]
fn blank_envelope_aborts_submit_withdrawal_without_reserving_usage() {
    let (env, _contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = funded_token(&env, &accounts.user1, DEPOSIT_AMOUNT);
    client.deposit(&token, &accounts.user1, &DEPOSIT_AMOUNT);
    install_withdrawal_limit(&client, &accounts, &token);
    let count_before = client.get_tx_count();
    env.set_auths(&[]);

    assert_auth_abort!(client.try_submit_withdrawal(
        &accounts.user1,
        &token,
        &accounts.arbiter,
        &(DEPOSIT_AMOUNT / 2)
    ));
    assert_eq!(client.get_tx_count(), count_before);
    assert_eq!(client.get_window_usage(&token), 0);
    assert_eq!(client.balance(&token), DEPOSIT_AMOUNT);
}

#[test]
fn submit_withdrawal_authorization_tree_is_the_owner_entrypoint_frame() {
    let (env, contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = funded_token(&env, &accounts.user1, DEPOSIT_AMOUNT);
    client.deposit(&token, &accounts.user1, &DEPOSIT_AMOUNT);
    install_withdrawal_limit(&client, &accounts, &token);

    client.submit_withdrawal(
        &accounts.user1,
        &token,
        &accounts.arbiter,
        &(DEPOSIT_AMOUNT / 2),
    );

    assert_eq!(
        env.auths(),
        [(
            accounts.user1.clone(),
            AuthorizedInvocation {
                function: AuthorizedFunction::Contract((
                    contract_id.clone(),
                    Symbol::new(&env, "submit_withdrawal"),
                    (
                        &accounts.user1,
                        &token,
                        &accounts.arbiter,
                        DEPOSIT_AMOUNT / 2,
                    )
                        .into_val(&env),
                )),
                sub_invocations: std::vec![],
            },
        )],
    );
}

// -----------------------------------------------------------------------
// set_withdrawal_limit / remove_withdrawal_limit — owner-only policy txs
// -----------------------------------------------------------------------

#[test]
fn set_withdrawal_limit_accepts_owner_signature() {
    let (env, contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = Address::generate(&env);

    env.mock_auths(&[MockAuth {
        address: &accounts.user1,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "set_withdrawal_limit",
            args: (&accounts.user1, &token, LIMIT_AMOUNT, WINDOW_SECONDS).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let tx_id = client
        .try_set_withdrawal_limit(&accounts.user1, &token, &LIMIT_AMOUNT, &WINDOW_SECONDS)
        .expect("outer ok")
        .expect("contract ok");
    assert_eq!(client.get_tx(&tx_id).submitter, accounts.user1);
    assert_eq!(client.get_tx_count(), 1);
    assert_eq!(
        client.get_tx(&tx_id).kind,
        TxKind::LimitChange(LimitChange::Set(WithdrawalLimit {
            token: token.clone(),
            amount: LIMIT_AMOUNT,
            window_seconds: WINDOW_SECONDS,
        }))
    );
    assert_eq!(client.get_withdrawal_limit(&token), None);
}

#[test]
fn set_withdrawal_limit_rejects_wrong_owner_signature() {
    let (env, contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = Address::generate(&env);

    env.mock_auths(&[MockAuth {
        address: &accounts.deployer,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "set_withdrawal_limit",
            args: (&accounts.user1, &token, LIMIT_AMOUNT, WINDOW_SECONDS).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    assert_auth_abort!(client.try_set_withdrawal_limit(
        &accounts.user1,
        &token,
        &LIMIT_AMOUNT,
        &WINDOW_SECONDS
    ));
    assert_eq!(client.get_tx_count(), 0);
    assert_eq!(client.get_withdrawal_limit(&token), None);
}

#[test]
fn set_withdrawal_limit_rejects_replay_with_different_parameters() {
    let (env, contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = Address::generate(&env);

    env.mock_auths(&[MockAuth {
        address: &accounts.user1,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "set_withdrawal_limit",
            args: (&accounts.user1, &token, 1_000_i128, 3_600_u64).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    assert_auth_abort!(client.try_set_withdrawal_limit(
        &accounts.user1,
        &token,
        &1_500_i128,
        &7_200_u64
    ));
    assert_eq!(client.get_tx_count(), 0);
    assert_eq!(client.get_withdrawal_limit(&token), None);
}

#[test]
fn blank_envelope_aborts_set_withdrawal_limit_without_writing_tx() {
    let (env, _contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = Address::generate(&env);
    env.set_auths(&[]);

    assert_auth_abort!(client.try_set_withdrawal_limit(
        &accounts.user1,
        &token,
        &LIMIT_AMOUNT,
        &WINDOW_SECONDS
    ));
    assert_eq!(client.get_tx_count(), 0);
    assert_eq!(client.get_withdrawal_limit(&token), None);
}

#[test]
fn remove_withdrawal_limit_accepts_owner_signature() {
    let (env, contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = Address::generate(&env);
    install_withdrawal_limit(&client, &accounts, &token);
    let active_limit = client.get_withdrawal_limit(&token);

    env.mock_auths(&[MockAuth {
        address: &accounts.user1,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "remove_withdrawal_limit",
            args: (&accounts.user1, &token).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let tx_id = client
        .try_remove_withdrawal_limit(&accounts.user1, &token)
        .expect("outer ok")
        .expect("contract ok");
    assert_eq!(client.get_tx(&tx_id).submitter, accounts.user1);
    assert_eq!(client.get_tx_count(), 2);
    assert_eq!(
        client.get_tx(&tx_id).kind,
        TxKind::LimitChange(LimitChange::Remove(token.clone()))
    );
    assert_eq!(client.get_withdrawal_limit(&token), active_limit);
}

#[test]
fn remove_withdrawal_limit_rejects_wrong_owner_signature() {
    let (env, contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = Address::generate(&env);
    install_withdrawal_limit(&client, &accounts, &token);
    let count_before = client.get_tx_count();
    let active_limit = client.get_withdrawal_limit(&token);

    env.mock_auths(&[MockAuth {
        address: &accounts.deployer,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "remove_withdrawal_limit",
            args: (&accounts.user1, &token).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    assert_auth_abort!(client.try_remove_withdrawal_limit(&accounts.user1, &token));
    assert_eq!(client.get_tx_count(), count_before);
    assert_eq!(client.get_withdrawal_limit(&token), active_limit);
}

#[test]
fn remove_withdrawal_limit_rejects_signature_replayed_for_another_token() {
    let (env, contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = Address::generate(&env);
    let other_token = Address::generate(&env);
    install_withdrawal_limit(&client, &accounts, &token);
    let count_before = client.get_tx_count();

    env.mock_auths(&[MockAuth {
        address: &accounts.user1,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "remove_withdrawal_limit",
            args: (&accounts.user1, &token).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    assert_auth_abort!(client.try_remove_withdrawal_limit(&accounts.user1, &other_token));
    assert_eq!(client.get_tx_count(), count_before);
    assert!(client.get_withdrawal_limit(&token).is_some());
    assert_eq!(client.get_withdrawal_limit(&other_token), None);
}

#[test]
fn blank_envelope_aborts_remove_withdrawal_limit_without_writing_tx() {
    let (env, _contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = Address::generate(&env);
    install_withdrawal_limit(&client, &accounts, &token);
    let count_before = client.get_tx_count();
    let active_limit = client.get_withdrawal_limit(&token);
    env.set_auths(&[]);

    assert_auth_abort!(client.try_remove_withdrawal_limit(&accounts.user1, &token));
    assert_eq!(client.get_tx_count(), count_before);
    assert_eq!(client.get_withdrawal_limit(&token), active_limit);
}

#[test]
fn set_withdrawal_limit_rejects_signature_replayed_for_different_token() {
    let (env, contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = Address::generate(&env);
    let other_token = Address::generate(&env);

    // user1 arms a signature for setting a limit on `token`, but the
    // invocation runs with `other_token`.
    env.mock_auths(&[MockAuth {
        address: &accounts.user1,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "set_withdrawal_limit",
            args: (&accounts.user1, &token, LIMIT_AMOUNT, WINDOW_SECONDS).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    assert_auth_abort!(client.try_set_withdrawal_limit(
        &accounts.user1,
        &other_token,
        &LIMIT_AMOUNT,
        &WINDOW_SECONDS
    ));
    assert_eq!(client.get_tx_count(), 0);
    assert_eq!(client.get_withdrawal_limit(&token), None);
    assert_eq!(client.get_withdrawal_limit(&other_token), None);
}

#[test]
fn set_withdrawal_limit_rejects_signature_replayed_for_different_amount() {
    let (env, contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = Address::generate(&env);

    // user1 arms a signature for 1_000, but invocation is called with 2_000.
    env.mock_auths(&[MockAuth {
        address: &accounts.user1,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "set_withdrawal_limit",
            args: (&accounts.user1, &token, 1_000_i128, WINDOW_SECONDS).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    assert_auth_abort!(client.try_set_withdrawal_limit(
        &accounts.user1,
        &token,
        &2_000_i128,
        &WINDOW_SECONDS
    ));
    assert_eq!(client.get_tx_count(), 0);
    assert_eq!(client.get_withdrawal_limit(&token), None);
}

#[test]
fn set_withdrawal_limit_rejects_signature_replayed_for_different_window() {
    let (env, contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = Address::generate(&env);

    // user1 arms a signature for 3_600s window, but invocation is called with 7_200s.
    env.mock_auths(&[MockAuth {
        address: &accounts.user1,
        invoke: &MockAuthInvoke {
            contract: &contract_id,
            fn_name: "set_withdrawal_limit",
            args: (&accounts.user1, &token, LIMIT_AMOUNT, 3_600_u64).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    assert_auth_abort!(client.try_set_withdrawal_limit(
        &accounts.user1,
        &token,
        &LIMIT_AMOUNT,
        &7_200_u64
    ));
    assert_eq!(client.get_tx_count(), 0);
    assert_eq!(client.get_withdrawal_limit(&token), None);
}

#[test]
fn set_withdrawal_limit_by_a_non_owner_is_unauthorized_even_under_mocked_auths() {
    let (env, _contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = Address::generate(&env);

    // Under blanket mocking the identity check is what rejects: the
    // non-owner deployer gets the typed error, not a host abort.
    let err = client
        .try_set_withdrawal_limit(&accounts.deployer, &token, &LIMIT_AMOUNT, &WINDOW_SECONDS)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, soroban_forge_shared_utils::ForgeError::Unauthorized);
    assert_eq!(client.get_tx_count(), 0);
    assert_eq!(client.get_withdrawal_limit(&token), None);
}

#[test]
fn set_withdrawal_limit_authorization_tree_is_the_owner_entrypoint_frame() {
    let (env, contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = Address::generate(&env);

    client.set_withdrawal_limit(&accounts.user1, &token, &LIMIT_AMOUNT, &WINDOW_SECONDS);

    // The tree is the submitter's entrypoint frame only — no token transfer
    // is involved in proposing a limit change.
    assert_eq!(
        env.auths(),
        [(
            accounts.user1.clone(),
            AuthorizedInvocation {
                function: AuthorizedFunction::Contract((
                    contract_id.clone(),
                    Symbol::new(&env, "set_withdrawal_limit"),
                    (&accounts.user1, &token, LIMIT_AMOUNT, WINDOW_SECONDS).into_val(&env),
                )),
                sub_invocations: std::vec![],
            },
        )],
    );
}

#[test]
fn remove_withdrawal_limit_by_a_non_owner_is_unauthorized_even_under_mocked_auths() {
    let (env, _contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = Address::generate(&env);
    install_withdrawal_limit(&client, &accounts, &token);
    let count_before = client.get_tx_count();

    let err = client
        .try_remove_withdrawal_limit(&accounts.deployer, &token)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, soroban_forge_shared_utils::ForgeError::Unauthorized);
    assert_eq!(client.get_tx_count(), count_before);
    assert!(client.get_withdrawal_limit(&token).is_some());
}

#[test]
fn remove_withdrawal_limit_authorization_tree_is_the_owner_entrypoint_frame() {
    let (env, contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = Address::generate(&env);
    install_withdrawal_limit(&client, &accounts, &token);

    client.remove_withdrawal_limit(&accounts.user1, &token);

    // The tree is the submitter's entrypoint frame only.
    assert_eq!(
        env.auths(),
        [(
            accounts.user1.clone(),
            AuthorizedInvocation {
                function: AuthorizedFunction::Contract((
                    contract_id.clone(),
                    Symbol::new(&env, "remove_withdrawal_limit"),
                    (&accounts.user1, &token).into_val(&env),
                )),
                sub_invocations: std::vec![],
            },
        )],
    );
}

#[test]
fn get_withdrawal_limit_is_callable_without_authorization() {
    let (env, _contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = Address::generate(&env);
    install_withdrawal_limit(&client, &accounts, &token);

    // Blank envelope: read-only query requires zero signatures.
    env.set_auths(&[]);

    let limit = client.get_withdrawal_limit(&token);
    assert_eq!(
        limit,
        Some(WithdrawalLimit {
            token: token.clone(),
            amount: LIMIT_AMOUNT,
            window_seconds: WINDOW_SECONDS,
        })
    );
    assert_eq!(env.auths().len(), 0);

    // Querying an unconfigured token also requires no authorization and returns None.
    let unconfigured_token = Address::generate(&env);
    assert_eq!(client.get_withdrawal_limit(&unconfigured_token), None);
}

#[test]
fn blank_envelope_aborts_set_withdrawal_limit_and_preserves_policy_and_window_state() {
    let (env, _contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = funded_token(&env, &accounts.user1, DEPOSIT_AMOUNT);
    client.deposit(&token, &accounts.user1, &DEPOSIT_AMOUNT);

    // Establish an initial active limit of 400.
    let initial_limit = 400_i128;
    let tx_id =
        client.set_withdrawal_limit(&accounts.user1, &token, &initial_limit, &WINDOW_SECONDS);
    client.confirm(&tx_id, &accounts.user2);
    client.confirm(&tx_id, &accounts.user3);
    client.execute(&tx_id);
    assert_eq!(client.get_tx_count(), 1);
    assert_eq!(
        client.get_withdrawal_limit(&token),
        Some(WithdrawalLimit {
            token: token.clone(),
            amount: initial_limit,
            window_seconds: WINDOW_SECONDS,
        })
    );

    // Blank envelope: attempting to propose a different limit aborts.
    env.set_auths(&[]);
    assert_auth_abort!(client.try_set_withdrawal_limit(
        &accounts.user1,
        &token,
        &800_i128,
        &WINDOW_SECONDS
    ));

    // Policy record, tx counter, and window usage are untouched.
    assert_eq!(client.get_tx_count(), 1);
    assert_eq!(
        client.get_withdrawal_limit(&token),
        Some(WithdrawalLimit {
            token: token.clone(),
            amount: initial_limit,
            window_seconds: WINDOW_SECONDS,
        })
    );
    assert_eq!(client.get_window_usage(&token), 0);

    // Subsequent withdrawal adheres strictly to the existing 400 limit:
    // Submitting 500 fails with LimitExceeded.
    env.mock_all_auths();
    let err = client
        .try_submit_withdrawal(&accounts.user1, &token, &accounts.arbiter, &500_i128)
        .unwrap_err()
        .unwrap();
    assert_eq!(
        err,
        soroban_forge_shared_utils::ForgeError::WithdrawalLimitExceeded
    );

    // Submitting 300 (within the 400 limit) succeeds and records usage.
    let w_tx = client
        .try_submit_withdrawal(&accounts.user1, &token, &accounts.arbiter, &300_i128)
        .expect("outer ok")
        .expect("contract ok");
    assert_eq!(w_tx, 2);
    assert_eq!(client.get_window_usage(&token), 300);
}

#[test]
fn blank_envelope_aborts_remove_withdrawal_limit_and_preserves_policy_and_window_state() {
    let (env, _contract_id, client, accounts) = setup!();
    initialize_wallet(&env, &client, &accounts);
    let token = funded_token(&env, &accounts.user1, DEPOSIT_AMOUNT);
    client.deposit(&token, &accounts.user1, &DEPOSIT_AMOUNT);

    // Establish an initial active limit of 400.
    let initial_limit = 400_i128;
    let tx_id =
        client.set_withdrawal_limit(&accounts.user1, &token, &initial_limit, &WINDOW_SECONDS);
    client.confirm(&tx_id, &accounts.user2);
    client.confirm(&tx_id, &accounts.user3);
    client.execute(&tx_id);
    let count_before = client.get_tx_count();
    assert_eq!(count_before, 1);

    // Blank envelope: attempting to propose removing the limit aborts.
    env.set_auths(&[]);
    assert_auth_abort!(client.try_remove_withdrawal_limit(&accounts.user1, &token));

    // Policy record, tx counter, and window usage are untouched.
    assert_eq!(client.get_tx_count(), count_before);
    assert_eq!(
        client.get_withdrawal_limit(&token),
        Some(WithdrawalLimit {
            token: token.clone(),
            amount: initial_limit,
            window_seconds: WINDOW_SECONDS,
        })
    );
    assert_eq!(client.get_window_usage(&token), 0);

    // Subsequent withdrawal confirms the limit is still actively enforced:
    // Submitting 500 fails with LimitExceeded.
    env.mock_all_auths();
    let err = client
        .try_submit_withdrawal(&accounts.user1, &token, &accounts.arbiter, &500_i128)
        .unwrap_err()
        .unwrap();
    assert_eq!(
        err,
        soroban_forge_shared_utils::ForgeError::WithdrawalLimitExceeded
    );

    // Submitting 300 (within the 400 limit) succeeds.
    let w_tx = client
        .try_submit_withdrawal(&accounts.user1, &token, &accounts.arbiter, &300_i128)
        .expect("outer ok")
        .expect("contract ok");
    assert_eq!(w_tx, 2);
    assert_eq!(client.get_window_usage(&token), 300);
}
