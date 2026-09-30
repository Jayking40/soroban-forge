//! Test-only contracts for cross-contract dispatch tests.
//!
//! Use [`MockTarget`] when a test needs a successful `execute(Bytes)` target,
//! payload recording, or deterministic revert control. Use the mock token
//! fixture for SEP-41 behavior; these targets intentionally do not model token
//! balances or production authorization.

use soroban_sdk::testutils::Address as _;
use soroban_sdk::{contracttype, Address, Env, Vec};

/// A fixed set of distinct mock accounts for use in tests.
///
/// Addresses are generated from the provided [`Env`] and are stable for the
/// lifetime of that environment, giving tests readable, named participants
/// (deployer, users, validator, arbiter) without hard-coding addresses.
#[contracttype]
pub struct TestAccounts {
    /// Account that deploys contracts and funds operations.
    pub deployer: Address,
    /// First regular user.
    pub user1: Address,
    /// Second regular user.
    pub user2: Address,
    /// Third regular user.
    pub user3: Address,
    /// A validator / signer role.
    pub validator: Address,
    /// A neutral arbiter role.
    pub arbiter: Address,
}

impl TestAccounts {
    /// Generate a fresh set of distinct mock addresses from `env`.
    pub fn generate(env: &Env) -> Self {
        TestAccounts {
            deployer: Address::generate(env),
            user1: Address::generate(env),
            user2: Address::generate(env),
            user3: Address::generate(env),
            validator: Address::generate(env),
            arbiter: Address::generate(env),
        }
    }

    /// All six accounts as a [`Vec`], useful for multi-party setup.
    pub fn all(&self, env: &Env) -> Vec<Address> {
        let mut v = Vec::new(env);
        v.push_back(self.deployer.clone());
        v.push_back(self.user1.clone());
        v.push_back(self.user2.clone());
        v.push_back(self.user3.clone());
        v.push_back(self.validator.clone());
        v.push_back(self.arbiter.clone());
        v
    }
}

/// Mock target contract for testing cross-contract invocations.
///
/// Records the execution count and last dispatched payload in instance storage.
#[soroban_sdk::contract]
pub struct MockTarget;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MockTargetKey {
    Count,
    LastPayload,
    Revert,
}

#[soroban_sdk::contractimpl]
impl MockTarget {
    /// Dispatched by contracts executing actions via cross-contract calls.
    pub fn execute(env: Env, payload: soroban_sdk::Bytes) {
        if env
            .storage()
            .instance()
            .get(&MockTargetKey::Revert)
            .unwrap_or(false)
        {
            panic!("mock target reverted");
        }
        let count: u32 = env
            .storage()
            .instance()
            .get(&MockTargetKey::Count)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&MockTargetKey::Count, &(count + 1));
        env.storage()
            .instance()
            .set(&MockTargetKey::LastPayload, &payload);
    }

    /// Make subsequent `execute` calls revert when enabled.
    pub fn set_revert(env: Env, revert: bool) {
        env.storage()
            .instance()
            .set(&MockTargetKey::Revert, &revert);
    }

    /// Read the number of times `execute` was called.
    pub fn count(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&MockTargetKey::Count)
            .unwrap_or(0)
    }

    /// Read the last payload dispatched to `execute`.
    pub fn last_payload(env: Env) -> Option<soroban_sdk::Bytes> {
        env.storage().instance().get(&MockTargetKey::LastPayload)
    }
}

/// A mock target whose `execute` panics, simulating a target revert.
#[soroban_sdk::contract]
pub struct RevertingTarget;

#[soroban_sdk::contractimpl]
impl RevertingTarget {
    /// Panics on invocation, simulating a target revert.
    pub fn execute(_env: Env, _payload: soroban_sdk::Bytes) {
        panic!("target reverted");
    }
}

#[cfg(test)]
mod tests {
    use super::{MockTarget, MockTargetClient};
    use soroban_sdk::{Bytes, Env};

    #[test]
    fn target_succeeds_by_default_and_records_payload() {
        let env = Env::default();
        let target = env.register(MockTarget, ());
        let client = MockTargetClient::new(&env, &target);
        let payload = Bytes::from_slice(&env, b"first");

        client.execute(&payload);

        assert_eq!(client.count(), 1);
        assert_eq!(client.last_payload(), Some(payload));
    }

    #[test]
    fn forced_revert_is_controllable_and_does_not_record_failed_payload() {
        let env = Env::default();
        let target = env.register(MockTarget, ());
        let client = MockTargetClient::new(&env, &target);
        client.set_revert(&true);

        assert!(client
            .try_execute(&Bytes::from_slice(&env, b"revert"))
            .is_err());
        assert_eq!(client.count(), 0);
        assert_eq!(client.last_payload(), None);

        client.set_revert(&false);
        client.execute(&Bytes::from_slice(&env, b"success"));
        assert_eq!(client.count(), 1);
    }

    #[test]
    fn records_the_most_recent_payload_across_multiple_calls() {
        let env = Env::default();
        let target = env.register(MockTarget, ());
        let client = MockTargetClient::new(&env, &target);
        let first = Bytes::from_slice(&env, b"first");
        let second = Bytes::from_slice(&env, b"second");

        client.execute(&first);
        client.execute(&second);

        assert_eq!(client.count(), 2);
        assert_eq!(client.last_payload(), Some(second));
    }

    #[test]
    fn instances_keep_revert_state_and_payloads_independent() {
        let env = Env::default();
        let first = env.register(MockTarget, ());
        let second = env.register(MockTarget, ());
        let first_client = MockTargetClient::new(&env, &first);
        let second_client = MockTargetClient::new(&env, &second);
        first_client.set_revert(&true);

        assert!(first_client
            .try_execute(&Bytes::from_slice(&env, b"blocked"))
            .is_err());
        second_client.execute(&Bytes::from_slice(&env, b"allowed"));

        assert_eq!(first_client.count(), 0);
        assert_eq!(second_client.count(), 1);
        assert_eq!(
            second_client.last_payload(),
            Some(Bytes::from_slice(&env, b"allowed"))
        );
    }
}
