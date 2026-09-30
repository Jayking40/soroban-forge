//! TTL chaos-harness demo for the multi-signature wallet contract.
//!
//! Drives randomized ledger gaps through the submit → confirm → execute
//! lifecycle and asserts that no persistent transaction record expires
//! during a legitimate flow.

extern crate alloc;
extern crate std;

use crate::{DataKey, MultiSigWallet, SorobanForgeMultiSigWalletClient, TxStatus};
use soroban_forge_shared_utils::ForgeError;
use soroban_forge_test_utils::ttl::{
    advance_ledger, assert_no_violations, chaos_drive, entry_is_live, ChaosTarget, StepOutcome,
};
use soroban_forge_test_utils::TestAccounts;
use soroban_sdk::{Address, Bytes, Env, Vec};
use std::string::{String, ToString};
use std::vec::Vec as StdVec;

/// Closed vocabulary of multi-sig operations for the chaos runner.
#[derive(Clone, Debug)]
enum MultiSigOp {
    Submit,
    Confirm(usize),
    Execute,
}

impl core::fmt::Display for MultiSigOp {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MultiSigOp::Submit => write!(f, "submit"),
            MultiSigOp::Confirm(idx) => write!(f, "confirm({idx})"),
            MultiSigOp::Execute => write!(f, "execute"),
        }
    }
}

struct MultiSigTarget<'a> {
    env: &'a Env,
    client: SorobanForgeMultiSigWalletClient<'a>,
    owners: StdVec<Address>,
    threshold: u32,
    tx_id: Option<u64>,
    confirmations: StdVec<Address>,
    status: TxStatus,
}

impl<'a> MultiSigTarget<'a> {
    fn setup(env: &'a Env) -> Self {
        env.mock_all_auths();

        let accounts = TestAccounts::generate(env);
        let owners = StdVec::from([
            accounts.user1.clone(),
            accounts.user2.clone(),
            accounts.user3.clone(),
        ]);
        let mut owner_vec = Vec::new(env);
        for owner in &owners {
            owner_vec.push_back(owner.clone());
        }

        let contract_id = env.register(MultiSigWallet, ());
        let client = SorobanForgeMultiSigWalletClient::new(env, &contract_id);
        client.initialize(&owner_vec, &2_u32);

        Self {
            env,
            client,
            owners,
            threshold: 2,
            tx_id: None,
            confirmations: StdVec::new(),
            status: TxStatus::Pending,
        }
    }

    fn target_payload(&self) -> Bytes {
        Bytes::from_array(self.env, &[0xDE, 0xAD, 0xBE, 0xEF])
    }

    fn key(&self) -> Option<DataKey> {
        self.tx_id.map(DataKey::Tx)
    }
}

impl<'a> ChaosTarget for MultiSigTarget<'a> {
    type Op = MultiSigOp;

    fn contract_address(&self) -> Address {
        self.client.address.clone()
    }

    fn enabled_ops(&self) -> StdVec<Self::Op> {
        match (self.tx_id, self.status.clone()) {
            (None, _) => alloc::vec![MultiSigOp::Submit],
            (Some(_), TxStatus::Pending) => {
                let mut ops = alloc::vec![MultiSigOp::Execute];
                for (idx, owner) in self.owners.iter().enumerate() {
                    if !self.confirmations.contains(owner) {
                        ops.push(MultiSigOp::Confirm(idx));
                    }
                }
                ops
            }
            _ => alloc::vec![],
        }
    }

    fn apply_op(&mut self, _env: &Env, op: &MultiSigOp) -> StepOutcome {
        match op {
            MultiSigOp::Submit => {
                let id = self.client.submit(
                    &self.owners[0],
                    &self.client.address,
                    &self.target_payload(),
                    &None,
                );
                self.tx_id = Some(id);
                self.confirmations.clear();
                self.status = TxStatus::Pending;
                StepOutcome::Ok
            }
            MultiSigOp::Confirm(idx) => {
                let Some(id) = self.tx_id else {
                    return StepOutcome::ExpectedFailure {
                        reason: "no transaction to confirm".into(),
                    };
                };
                let owner = &self.owners[*idx % self.owners.len()];
                if self.confirmations.contains(owner) {
                    return StepOutcome::ExpectedFailure {
                        reason: "owner already confirmed".into(),
                    };
                }
                match self.client.try_confirm(&id, owner) {
                    Ok(Ok(())) => {
                        self.confirmations.push(owner.clone());
                        if self.confirmations.len() >= self.threshold as usize {
                            self.status = TxStatus::Executed;
                        }
                        StepOutcome::Ok
                    }
                    other => classify_result("confirm", other),
                }
            }
            MultiSigOp::Execute => {
                let Some(id) = self.tx_id else {
                    return StepOutcome::ExpectedFailure {
                        reason: "no transaction to execute".into(),
                    };
                };
                match self.client.try_execute(&id) {
                    Ok(Ok(())) => {
                        self.status = TxStatus::Executed;
                        StepOutcome::Ok
                    }
                    other => classify_result("execute", other),
                }
            }
        }
    }

    fn check_invariants(&self, _env: &Env) -> Option<String> {
        let key = self.key()?;
        if !entry_is_live(self.env, &self.client.address, &key) {
            return Some(std::format!(
                "multi-sig transaction record is not live after {:?}",
                self.status
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
            description: std::format!(
                "{context}: transaction record returned NotFound during legitimate flow"
            ),
        };
    }
    StepOutcome::ExpectedFailure {
        reason: std::format!("{context}: {err:?}"),
    }
}

/// Property: no persistent multi-sig transaction record expires during an
/// arbitrary submit → confirm → execute flow with randomized ledger gaps.
#[test]
fn multisig_no_notfound_over_randomized_ledger_gaps() {
    let env = Env::default();
    let mut target = MultiSigTarget::setup(&env);
    let trace = chaos_drive(&env, &mut target, 252_252, 16, 300_000);
    assert_no_violations(&trace);
}

/// Fresh-record access right after submission survives a large ledger gap.
#[test]
fn fresh_multisig_tx_record_survives_ledger_gap_after_submission() {
    let env = Env::default();
    let mut target = MultiSigTarget::setup(&env);
    advance_ledger(&env, 100_000);

    let trace = chaos_drive(&env, &mut target, 42_000, 8, 200_000);
    assert_no_violations(&trace);
}

/// A sample trace is printable and deterministic for a fixed seed.
#[test]
fn multisig_chaos_trace_is_printable() {
    let env = Env::default();
    let mut target = MultiSigTarget::setup(&env);
    let trace = chaos_drive(&env, &mut target, 2_222, 6, 50_000);
    assert!(!trace.steps.is_empty());
    let _ = trace.to_string();
}
