//! Deterministic ground-truth proposal, voting, and bond events.

extern crate std;

use crate::{DaoGovernance, SorobanForgeDaoGovernanceClient};
use serde_json::Value;
use soroban_forge_test_utils::{MockTarget, TestAccounts};
use soroban_sdk::testutils::{Address as _, Events as _, Ledger as _};
use soroban_sdk::token::StellarAssetClient;
use soroban_sdk::xdr::{self, WriteXdr};
use soroban_sdk::{Address, Bytes, Env};
use std::format;
use std::path::Path;
use std::string::ToString;
use std::vec::Vec;

const START: u64 = 1_000_000;
const DURATION: u64 = 1_000;
const BOND: i128 = 100;
const XDR_LIMITS: xdr::Limits = xdr::Limits {
    depth: 500,
    len: 0x1_000_000,
};
const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../indexer/fixtures");
const FIXTURE_FILE: &str = "dao-governance-events.json";

fn capture(env: &Env, contract: &Address, events: &mut Vec<Value>) {
    for (index, event) in env.events().all().events().iter().enumerate() {
        let xdr::ContractEventBody::V0(body) = &event.body;
        events.push(serde_json::json!({
            "type": "contract",
            "ledger": index,
            "ledgerClosedAt": "2026-09-23T00:00:00.000Z",
            "contractId": contract.to_string().to_string(),
            "id": format!("0-{index}"),
            "pagingToken": format!("0-{index}"),
            "inSuccessfulContractCall": true,
            "eventIndex": index,
            "transactionHash": "0000000000000000000000000000000000000000000000000000000000000000",
            "operationIndex": 0,
            "topic": body.topics.iter().map(|v| v.to_xdr_base64(XDR_LIMITS).unwrap()).collect::<Vec<_>>(),
            "value": body.data.to_xdr_base64(XDR_LIMITS).unwrap(),
        }));
    }
}

fn generate() -> Value {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(START);
    let admin = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin);
    let token = sac.address();
    let contract = env.register(DaoGovernance, ());
    let client = SorobanForgeDaoGovernanceClient::new(&env, &contract);
    let accounts = TestAccounts::generate(&env);
    client.initialize(&token);
    client.configure_bond(&token, &BOND, &accounts.deployer);
    let token_admin = StellarAssetClient::new(&env, &token);
    token_admin.mint(&accounts.user1, &1_000_i128);
    token_admin.mint(&accounts.user2, &250_i128);
    let target = Address::generate(&env);
    env.register_at(&target, MockTarget, ());
    let payload = Bytes::from_slice(&env, b"fixture");

    let proposal_id = client.propose(
        &accounts.user1,
        &target,
        &payload,
        &DURATION,
        &soroban_sdk::Vec::new(&env),
        &None,
    );
    let mut events = Vec::new();
    capture(&env, &contract, &mut events);
    client.vote(&proposal_id, &accounts.user2, &true);
    capture(&env, &contract, &mut events);
    env.ledger().set_timestamp(START + DURATION + 1);
    client.execute(&proposal_id);
    capture(&env, &contract, &mut events);
    client.execute(&proposal_id);
    capture(&env, &contract, &mut events);

    serde_json::json!({
        "schema": 1,
        "description": "Deterministic proposal, vote, finalisation, and bond lifecycle events from Soroban Forge.",
        "contract": { "name": "soroban-forge-dao-governance", "id": contract.to_string().to_string() },
        "events": events,
    })
}

#[test]
fn indexer_fixtures_regenerate_and_are_stable() {
    let fixture = generate();
    assert_eq!(
        fixture,
        generate(),
        "fixture generation is not deterministic"
    );
    let path = Path::new(FIXTURE_DIR).join(FIXTURE_FILE);
    std::fs::create_dir_all(FIXTURE_DIR).unwrap();
    let pretty = format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap());
    if std::fs::read_to_string(&path).ok().as_deref() != Some(pretty.as_str()) {
        std::fs::write(&path, pretty).unwrap();
        panic!("fixture changed; review and commit {}", path.display());
    }
}
