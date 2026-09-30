//! TTL chaos-harness demo for the escrow contract.
//!
//! Exercises the shared [`soroban_forge_test_utils::ttl`] harness against
//! the escrow lifecycle. The runner inserts arbitrary ledger advances
//! between legitimate operations and asserts that no persistent escrow
//! record becomes `NotFound` during the flow.

extern crate alloc;
extern crate std;

use crate::{DataKey, Escrow, EscrowStatus, SorobanForgeEscrowClient};
use soroban_forge_shared_utils::ForgeError;
use soroban_forge_test_utils::ttl::{
    advance_ledger, assert_no_violations, chaos_drive, entry_is_live, ChaosTarget, StepOutcome,
};
use soroban_forge_test_utils::TestAccounts;
use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::token::StellarAssetClient;
use soroban_sdk::{Address, Env};
use std::string::{String, ToString};
use std::vec::Vec;

const START_TIME: u64 = 1_000_000;
const TIMEOUT: u64 = 10_000;
const AMOUNT: i128 = 1_000;

/// A closed vocabulary of escrow operations used by the chaos runner.
#[derive(Clone, Debug)]
enum EscrowOp {
    Create,
    Deposit,
    Release,
    Refund,
}

impl core::fmt::Display for EscrowOp {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            EscrowOp::Create => write!(f, "create"),
            EscrowOp::Deposit => write!(f, "deposit"),
            EscrowOp::Release => write!(f, "release"),
            EscrowOp::Refund => write!(f, "refund"),
        }
    }
}

/// Contract-specific chaos target for escrow.
struct EscrowTarget<'a> {
    env: &'a Env,
    client: SorobanForgeEscrowClient<'a>,
    token: Address,
    accounts: TestAccounts,
    escrow_id: Option<u64>,
    status: EscrowStatus,
}

impl<'a> EscrowTarget<'a> {
    fn setup(env: &'a Env) -> Self {
        env.mock_all_auths();
        env.ledger().set_timestamp(START_TIME);

        let admin = Address::generate(env);
        let sac = env.register_stellar_asset_contract_v2(admin);
        let token = sac.address();
        let accounts = TestAccounts::generate(env);
        StellarAssetClient::new(env, &token).mint(&accounts.user1, &AMOUNT);

        let contract_id = env.register(Escrow, ());
        let client = SorobanForgeEscrowClient::new(env, &contract_id);

        Self {
            env,
            client,
            token,
            accounts,
            escrow_id: None,
            status: EscrowStatus::Pending,
        }
    }

    fn key(&self) -> Option<DataKey> {
        self.escrow_id.map(DataKey::Escrow)
    }
}

impl<'a> ChaosTarget for EscrowTarget<'a> {
    type Op = EscrowOp;

    fn contract_address(&self) -> Address {
        self.client.address.clone()
    }

    fn enabled_ops(&self) -> Vec<Self::Op> {
        match self.status {
            // Before creation only create is valid.
            EscrowStatus::Pending if self.escrow_id.is_none() => alloc::vec![EscrowOp::Create],
            // After creation but before funding we can deposit.
            EscrowStatus::Pending => alloc::vec![EscrowOp::Deposit],
            // Once funded we can release or refund.
            EscrowStatus::Funded => alloc::vec![EscrowOp::Release, EscrowOp::Refund],
            // Terminal states have no further legitimate ops.
            EscrowStatus::Completed
            | EscrowStatus::Refunded
            | EscrowStatus::Disputed
            | EscrowStatus::Cancelled => alloc::vec![],
        }
    }

    fn apply_op(&mut self, _env: &Env, op: &EscrowOp) -> StepOutcome {
        let id = match self.escrow_id {
            None => {
                // Only Create is valid here; enabled_ops guarantees it.
                let id = self.client.create_escrow(
                    &self.accounts.user1,
                    &self.accounts.user2,
                    &self.accounts.arbiter,
                    &self.token,
                    &AMOUNT,
                    &TIMEOUT,
                );
                self.escrow_id = Some(id);
                return StepOutcome::Ok;
            }
            Some(id) => id,
        };

        match op {
            EscrowOp::Create => StepOutcome::ExpectedFailure {
                reason: "escrow already created".into(),
            },
            EscrowOp::Deposit => classify_result("deposit", self.client.try_deposit(&id)),
            EscrowOp::Release => classify_result("release", self.client.try_release(&id)),
            EscrowOp::Refund => classify_result("refund", self.client.try_refund(&id)),
        }
    }

    fn check_invariants(&self, _env: &Env) -> Option<String> {
        let key = self.key()?;
        if !entry_is_live(self.env, &self.client.address, &key) {
            let key_label = match key {
                DataKey::Escrow(id) => std::format!("Escrow({id})"),
                _ => "other".into(),
            };
            return Some(std::format!(
                "escrow entry {key_label} is not live after {status:?}",
                status = self.status
            ));
        }
        None
    }
}

fn classify_result<T>(
    context: &str,
    res: Result<
        Result<T, soroban_sdk::ConversionError>,
        Result<ForgeError, soroban_sdk::InvokeError>,
    >,
) -> StepOutcome {
    match res {
        Ok(Ok(_)) => StepOutcome::Ok,
        Ok(Err(_)) => StepOutcome::Violation {
            description: std::format!("{context}: conversion error"),
        },
        Err(Ok(forge_err)) => classify_forge_error(context, forge_err),
        Err(Err(_)) => StepOutcome::Violation {
            description: std::format!("{context}: invoke error"),
        },
    }
}

fn classify_forge_error(context: &str, err: ForgeError) -> StepOutcome {
    if err == ForgeError::NotFound {
        return StepOutcome::Violation {
            description: std::format!("{context}: record returned NotFound during legitimate flow"),
        };
    }
    StepOutcome::ExpectedFailure {
        reason: std::format!("{context}: {err:?}"),
    }
}

/// Pin a fresh-record access right after creation with a large ledger gap.
/// This is the failure mode PR #179 hit: a record written and then read
/// across a TTL boundary must still be live.
#[test]
fn fresh_escrow_record_survives_ledger_gap_after_creation() {
    let env = Env::default();
    let mut target = EscrowTarget::setup(&env);
    advance_ledger(&env, 100_000);

    let trace = chaos_drive(&env, &mut target, 42_000, 8, 200_000);
    assert_no_violations(&trace);
}

/// Property: no persistent escrow entry expires during an arbitrary
/// create → deposit → release/refund flow with randomized ledger gaps.
#[test]
fn escrow_no_notfound_over_randomized_ledger_gaps() {
    let env = Env::default();
    let mut target = EscrowTarget::setup(&env);
    let trace = chaos_drive(&env, &mut target, 252_252, 16, 300_000);
    assert_no_violations(&trace);
}

/// A sample chaos trace is printable and deterministic for a fixed seed.
#[test]
fn escrow_chaos_trace_is_printable() {
    let env = Env::default();
    let mut target = EscrowTarget::setup(&env);
    let trace = chaos_drive(&env, &mut target, 1_111, 6, 50_000);
    // The trace must be deterministic and contain at least one step.
    assert!(!trace.steps.is_empty());
    let _ = trace.to_string();
}
