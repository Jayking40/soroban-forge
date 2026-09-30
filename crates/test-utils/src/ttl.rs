//! TTL chaos-harness helpers.
//!
//! Provides deterministic ledger-sequence manipulation, persistent-entry
//! liveness introspection, and a seeded chaos runner for driving
//! randomized operation/advance schedules against a contract under test.
//!
//! The harness is intentionally generic: a target contract implements the
//! [`ChaosTarget`] trait with a closed vocabulary of operations, and the
//! runner replays a deterministic schedule. The same seed always produces the
//! same trace, so regressions are reproducible.
//!
//! # Ledger manipulation
//!
//! [`advance_ledger`] moves the test [`Env`]'s ledger sequence forward by a
//! given number of ledgers. It guards monotonicity so a buggy or adversarial
//! schedule cannot rewind ledger time.
//!
//! # Entry liveness
//!
//! [`entry_ttl`] and [`entry_is_live`] introspect a contract's persistent
//! storage from the test harness. They run inside the target contract's
//! frame with [`Env::as_contract`], so they report the same TTL the host
//! uses when deciding whether an entry is live.
//!
//! # Chaos runner
//!
//! [`chaos_drive`] consumes a seeded schedule, applies each selected
//! operation, advances the ledger between calls, and records the outcome.
//! The caller's [`ChaosTarget::check_invariants`] hook is invoked after every
//! step so the contract can assert the "no `NotFound` during legitimate
//! flows" property or detect recreate-default corruption.

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;
use soroban_sdk::testutils::{storage::Persistent as _, Ledger as _};
use soroban_sdk::{Address, Env, IntoVal, Val};

/// Number of ledgers in a day (approximately, based on 5-second ledgers).
///
/// Mirrors the policy constant in `soroban_forge_shared_utils::ttl` so chaos
/// tests reason about the same time units as production code.
pub const DAY_IN_LEDGERS: u32 = 17_280;

/// Default TTL horizon applied by workspace contracts when bumping entries
/// (30 days of ledgers).
pub const BUMP_AMOUNT: u32 = 30 * DAY_IN_LEDGERS;

/// Bump threshold used by workspace contracts: entries are only extended
/// when they are within one day of expiry.
pub const BUMP_THRESHOLD: u32 = BUMP_AMOUNT - DAY_IN_LEDGERS;

/// Outcome of a single chaos step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StepOutcome {
    /// The operation completed without error.
    Ok,
    /// The operation failed with an error that is legitimate for the
    /// current state (e.g., a state-machine guard). The trace records the
    /// reason, but the run is not treated as a harness violation.
    ExpectedFailure {
        /// Human-readable reason recorded in the trace.
        reason: String,
    },
    /// The operation failed with an error that should not occur during a
    /// legitimate flow. The harness reports this as a violation.
    Violation {
        /// Description of the invariant that was broken.
        description: String,
    },
}

/// A single step in a chaos trace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChaosStep {
    /// Step index, starting at zero.
    pub step: usize,
    /// Display string for the operation that was attempted.
    pub op: String,
    /// Ledgers advanced immediately before the operation.
    pub ledger_advance: u32,
    /// Outcome of the operation.
    pub outcome: StepOutcome,
    /// Optional invariant violation detected after the step.
    pub invariant_check: Option<String>,
}

/// A complete, reproducible chaos run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChaosTrace {
    /// Seed used to generate the schedule.
    pub seed: u64,
    /// Steps in execution order.
    pub steps: Vec<ChaosStep>,
}

impl fmt::Display for ChaosTrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "ChaosTrace(seed={})", self.seed)?;
        for step in &self.steps {
            writeln!(
                f,
                "  [{:>3}] advance={:>6} op={} -> {:?}",
                step.step, step.ledger_advance, step.op, step.outcome
            )?;
            if let Some(ref violation) = step.invariant_check {
                writeln!(f, "        VIOLATION: {violation}")?;
            }
        }
        Ok(())
    }
}

/// Move the test environment's ledger sequence forward by `n` ledgers.
///
/// # Panics
///
/// Panics if `n` would cause the sequence number to overflow `u32`, or if
/// the resulting sequence is not strictly greater than the current
/// sequence (monotonicity guard).
pub fn advance_ledger(env: &Env, n: u32) {
    let current = env.ledger().sequence();
    let next = current
        .checked_add(n)
        .expect("ledger sequence must not overflow");
    assert!(next >= current, "ledger sequence must not move backwards");
    env.ledger()
        .with_mut(|ledger| ledger.sequence_number = next);
}

/// Return the remaining TTL of a persistent entry in `contract`'s storage.
///
/// Returns `0` when the entry is absent or its TTL has expired. The check
/// runs inside the contract's frame via [`Env::as_contract`] so it matches
/// the host's own view of the entry.
pub fn entry_ttl<K>(env: &Env, contract: &Address, key: &K) -> u32
where
    K: IntoVal<Env, Val>,
{
    env.as_contract(contract, || {
        if !env.storage().persistent().has(key) {
            return 0;
        }
        env.storage().persistent().get_ttl(key)
    })
}

/// Return `true` if a persistent entry exists in `contract`'s storage and
/// still has remaining TTL.
pub fn entry_is_live<K>(env: &Env, contract: &Address, key: &K) -> bool
where
    K: IntoVal<Env, Val>,
{
    entry_ttl(env, contract, key) > 0
}

/// Contract-specific driver for the chaos runner.
///
/// Implementors provide a closed vocabulary of operations and state-machine
/// logic. The runner handles scheduling and ledger advances; the target
/// decides which operations are currently valid and how to interpret
/// success and failure.
pub trait ChaosTarget {
    /// A printable, closed operation from the target's vocabulary.
    type Op: Clone + fmt::Debug + fmt::Display;

    /// The contract whose persistent storage is under test.
    fn contract_address(&self) -> Address;

    /// Operations that are valid to apply in the current state.
    ///
    /// Returning an empty vector stops the run early.
    fn enabled_ops(&self) -> Vec<Self::Op>;

    /// Apply one operation, returning an outcome that the runner records.
    fn apply_op(&mut self, env: &Env, op: &Self::Op) -> StepOutcome;

    /// Optional invariant check after each operation.
    ///
    /// Return `Some(description)` to flag a violation. This is where a
    /// target asserts that no required persistent entry expired and no
    /// recreate-default corruption occurred.
    fn check_invariants(&self, env: &Env) -> Option<String>;
}

/// Deterministic seeded LCG used to generate chaos schedules.
///
/// A minimal PRNG so the harness is self-contained and reproducible across
/// machines without relying on the test framework's RNG.
#[derive(Clone, Copy, Debug)]
pub struct Lcg64 {
    state: u64,
}

impl Lcg64 {
    /// Create a generator from `seed`. The same seed always yields the same
    /// sequence.
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Step the generator and return a `u64`.
    pub fn next_u64(&mut self) -> u64 {
        // Parameters from Numerical Recipes / PCG family; good enough for
        // generating test schedules, not for cryptography.
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005_u64)
            .wrapping_add(1_442_695_040_888_963_407_u64);
        self.state
    }

    /// Return a `u32` in the half-open range `[0, max)`.
    ///
    /// Returns `0` when `max == 0`.
    pub fn bounded(&mut self, max: u32) -> u32 {
        if max == 0 {
            return 0;
        }
        (self.next_u64() >> 32) as u32 % max
    }
}

/// Run a deterministic chaos schedule against `target`.
///
/// * `env` — the shared Soroban test environment.
/// * `target` — the contract-specific driver.
/// * `seed` — schedule seed; the same seed reproduces the same trace.
/// * `steps` — maximum number of operations to apply.
/// * `max_advance` — maximum ledger advance inserted before each operation.
///   The actual advance is uniformly random in `[0, max_advance)`.
///
/// The runner records every step, including expected failures. The trace
/// is printable and equality-comparable for determinism proofs.
///
/// # Panics
///
/// Panics if any enabled operation cannot be displayed as a string.
pub fn chaos_drive<T: ChaosTarget>(
    env: &Env,
    target: &mut T,
    seed: u64,
    steps: usize,
    max_advance: u32,
) -> ChaosTrace {
    let mut rng = Lcg64::new(seed);
    let mut trace = ChaosTrace {
        seed,
        steps: Vec::new(),
    };

    for step in 0..steps {
        let enabled = target.enabled_ops();
        if enabled.is_empty() {
            break;
        }

        let op_index = rng.bounded(enabled.len() as u32) as usize;
        let op = enabled[op_index].clone();
        let advance = rng.bounded(max_advance);

        advance_ledger(env, advance);
        let outcome = target.apply_op(env, &op);
        let invariant_check = target.check_invariants(env);

        trace.steps.push(ChaosStep {
            step,
            op: op.to_string(),
            ledger_advance: advance,
            outcome,
            invariant_check,
        });
    }

    trace
}

/// Convenience helper: assert that `trace` contains no violations.
///
/// A violation is either a [`StepOutcome::Violation`] from an operation, or
/// a non-`None` invariant check. The assertion prints the full trace on
/// failure so the seed can be replayed.
pub fn assert_no_violations(trace: &ChaosTrace) {
    let mut failures = Vec::new();
    for step in &trace.steps {
        if let StepOutcome::Violation { ref description } = step.outcome {
            failures.push(format!(
                "step {} op={} operation violation: {description}",
                step.step, step.op
            ));
        }
        if let Some(ref violation) = step.invariant_check {
            failures.push(format!(
                "step {} op={} invariant violation: {violation}",
                step.step, step.op
            ));
        }
    }
    if !failures.is_empty() {
        panic!(
            "chaos trace contained violations:\n{trace}\n{}",
            failures.join("\n")
        );
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    extern crate std;

    use alloc::format;
    use std::string::String;

    use super::{
        advance_ledger, assert_no_violations, chaos_drive, entry_is_live, entry_ttl, ChaosTarget,
        StepOutcome, BUMP_AMOUNT, BUMP_THRESHOLD,
    };
    use soroban_sdk::{contract, contractimpl, contracttype, Address, Env};

    // -------------------------------------------------------------------
    // Ledger manipulation and TTL introspection
    // -------------------------------------------------------------------

    #[contract]
    struct TtlProbe;

    #[contracttype]
    #[derive(Clone, Debug, Eq, PartialEq)]
    enum ProbeKey {
        Value,
    }

    #[contractimpl]
    impl TtlProbe {
        fn write(env: Env) {
            env.storage().persistent().set(&ProbeKey::Value, &42_u32);
            env.storage()
                .persistent()
                .extend_ttl(&ProbeKey::Value, BUMP_THRESHOLD, BUMP_AMOUNT);
        }
    }

    fn setup_probe() -> (Env, Address) {
        let env = Env::default();
        let contract = env.register(TtlProbe, ());
        (env, contract)
    }

    #[test]
    fn advance_ledger_moves_sequence_forward() {
        let (env, _contract) = setup_probe();
        let before = env.ledger().sequence();
        advance_ledger(&env, 1_000);
        assert_eq!(env.ledger().sequence(), before + 1_000);
    }

    #[test]
    fn entry_ttl_matches_bump_horizon_after_write() {
        let (env, contract) = setup_probe();

        env.as_contract(&contract, || TtlProbe::write(env.clone()));

        assert!(entry_is_live(&env, &contract, &ProbeKey::Value));
        assert_eq!(entry_ttl(&env, &contract, &ProbeKey::Value), BUMP_AMOUNT);
    }

    #[test]
    fn entry_ttl_decays_as_ledger_advances() {
        let (env, contract) = setup_probe();

        env.as_contract(&contract, || TtlProbe::write(env.clone()));
        advance_ledger(&env, 10_000);

        assert_eq!(
            entry_ttl(&env, &contract, &ProbeKey::Value),
            BUMP_AMOUNT - 10_000
        );
        assert!(entry_is_live(&env, &contract, &ProbeKey::Value));
    }

    #[test]
    fn missing_entry_reports_zero_ttl_and_not_live() {
        let (env, contract) = setup_probe();
        assert!(!entry_is_live(&env, &contract, &ProbeKey::Value));
        assert_eq!(entry_ttl(&env, &contract, &ProbeKey::Value), 0);
    }

    #[test]
    fn entry_reaches_end_of_horizon() {
        let (env, contract) = setup_probe();

        env.as_contract(&contract, || TtlProbe::write(env.clone()));
        // Advance to the last ledger the entry is still live.
        advance_ledger(&env, BUMP_AMOUNT - 1);

        assert_eq!(entry_ttl(&env, &contract, &ProbeKey::Value), 1);
        assert!(entry_is_live(&env, &contract, &ProbeKey::Value));
    }

    // -------------------------------------------------------------------
    // Determinism: same seed yields the identical trace
    // -------------------------------------------------------------------

    #[contract]
    struct Counter;

    #[contracttype]
    #[derive(Clone, Debug, Eq, PartialEq)]
    enum CounterKey {
        Count,
    }

    #[contractimpl]
    impl Counter {
        fn inc(env: Env) {
            let current: u32 = env
                .storage()
                .persistent()
                .get(&CounterKey::Count)
                .unwrap_or(0);
            env.storage()
                .persistent()
                .set(&CounterKey::Count, &(current + 1));
            env.storage()
                .persistent()
                .extend_ttl(&CounterKey::Count, BUMP_THRESHOLD, BUMP_AMOUNT);
        }
    }

    #[derive(Clone, Debug)]
    enum CounterOp {
        Inc,
    }

    impl core::fmt::Display for CounterOp {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                CounterOp::Inc => write!(f, "inc"),
            }
        }
    }

    struct CounterTarget {
        contract: Address,
    }

    impl ChaosTarget for CounterTarget {
        type Op = CounterOp;

        fn contract_address(&self) -> Address {
            self.contract.clone()
        }

        fn enabled_ops(&self) -> super::Vec<Self::Op> {
            alloc::vec![CounterOp::Inc]
        }

        fn apply_op(&mut self, env: &Env, _op: &CounterOp) -> StepOutcome {
            env.as_contract(&self.contract, || Counter::inc(env.clone()));
            StepOutcome::Ok
        }

        fn check_invariants(&self, env: &Env) -> Option<String> {
            if !entry_is_live(env, &self.contract, &CounterKey::Count) {
                return Some("Counter entry is not live".into());
            }
            None
        }
    }

    #[test]
    fn chaos_drive_is_deterministic_for_same_seed() {
        let env = Env::default();
        let contract = env.register(Counter, ());
        let mut target = CounterTarget { contract };

        let trace_a = chaos_drive(&env, &mut target, 123_456, 16, 5_000);
        // Reset env and target state so the second run is independent.
        let env = Env::default();
        let contract = env.register(Counter, ());
        let mut target = CounterTarget { contract };
        let trace_b = chaos_drive(&env, &mut target, 123_456, 16, 5_000);

        assert_eq!(trace_a, trace_b);
    }

    #[test]
    fn different_seeds_produce_different_traces() {
        let env = Env::default();
        let contract = env.register(Counter, ());
        let mut target = CounterTarget { contract };
        let trace_a = chaos_drive(&env, &mut target, 123_456, 16, 5_000);

        let env = Env::default();
        let contract = env.register(Counter, ());
        let mut target = CounterTarget { contract };
        let trace_b = chaos_drive(&env, &mut target, 999_999, 16, 5_000);

        assert_ne!(trace_a, trace_b);
    }

    // -------------------------------------------------------------------
    // Recreate-default corruption detector
    // -------------------------------------------------------------------

    /// A buggy contract that "recreates" a default value when its persistent
    /// entry has expired, instead of failing with a `NotFound`-style error.
    /// The test harness simulates expiry by removing the entry between calls;
    /// the contract's `unwrap_or(0)` then hides the absence.
    #[contract]
    struct RecreateDefaultBug;

    #[contracttype]
    #[derive(Clone, Debug, Eq, PartialEq)]
    enum BugKey {
        Value,
    }

    #[contractimpl]
    impl RecreateDefaultBug {
        fn write(env: Env, value: u32) {
            env.storage().persistent().set(&BugKey::Value, &value);
        }

        /// Intentionally wrong: returns 0 when the entry is absent, which hides
        /// a missing record behind a recreated default.
        fn read(env: Env) -> u32 {
            env.storage().persistent().get(&BugKey::Value).unwrap_or(0)
        }
    }

    #[derive(Clone, Debug)]
    enum BugOp {
        Write,
        Read,
    }

    impl core::fmt::Display for BugOp {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                BugOp::Write => write!(f, "write"),
                BugOp::Read => write!(f, "read"),
            }
        }
    }

    struct BugTarget {
        contract: Address,
        reads_since_write: u32,
        last_written: Option<u32>,
    }

    impl ChaosTarget for BugTarget {
        type Op = BugOp;

        fn contract_address(&self) -> Address {
            self.contract.clone()
        }

        fn enabled_ops(&self) -> super::Vec<Self::Op> {
            let mut ops = alloc::vec![];
            if self.last_written.is_none() {
                ops.push(BugOp::Write);
            }
            ops.push(BugOp::Read);
            ops
        }

        fn apply_op(&mut self, env: &Env, op: &BugOp) -> StepOutcome {
            match op {
                BugOp::Write => {
                    env.as_contract(&self.contract, || {
                        RecreateDefaultBug::write(env.clone(), 42)
                    });
                    self.last_written = Some(42);
                    self.reads_since_write = 0;
                    StepOutcome::Ok
                }
                BugOp::Read => {
                    // Simulate a TTL lapse on the second read after a write by
                    // deleting the entry from the test harness. A disciplined
                    // contract would still fail loudly; this one returns a
                    // default because it uses `unwrap_or(0)`.
                    self.reads_since_write += 1;
                    if self.reads_since_write == 2 {
                        env.as_contract(&self.contract, || {
                            env.storage().persistent().remove(&BugKey::Value)
                        });
                    }

                    let value =
                        env.as_contract(&self.contract, || RecreateDefaultBug::read(env.clone()));
                    if self.last_written == Some(42) && value != 42 {
                        return StepOutcome::Violation {
                            description: format!(
                                "recreate-default corruption: expected 42, read {value}"
                            ),
                        };
                    }
                    StepOutcome::Ok
                }
            }
        }

        fn check_invariants(&self, env: &Env) -> Option<String> {
            // The key should be live whenever we have written it.
            if self.last_written.is_some() && !entry_is_live(env, &self.contract, &BugKey::Value) {
                return Some("BugKey is not live after a write".into());
            }
            None
        }
    }

    #[test]
    fn chaos_catches_recreate_default_corruption() {
        let env = Env::default();
        let contract = env.register(RecreateDefaultBug, ());
        let mut target = BugTarget {
            contract,
            last_written: None,
            reads_since_write: 0,
        };

        // A seeded, short run that deterministically exercises write followed
        // by a second read. The harness detects both the absent entry and the
        // default value returned by the buggy contract.
        let trace = chaos_drive(&env, &mut target, 7_777, 4, 1_000);

        let has_violation = trace
            .steps
            .iter()
            .any(|step| matches!(step.outcome, StepOutcome::Violation { .. }));
        assert!(
            has_violation,
            "expected the harness to detect recreate-default corruption; trace:\n{trace}"
        );
    }

    #[test]
    fn assert_no_violations_passes_on_clean_trace() {
        let env = Env::default();
        let contract = env.register(Counter, ());
        let mut target = CounterTarget { contract };
        let trace = chaos_drive(&env, &mut target, 42, 8, 1_000);
        assert_no_violations(&trace);
    }

    #[test]
    #[should_panic(expected = "chaos trace contained violations")]
    fn assert_no_violations_fails_on_corruption_trace() {
        let env = Env::default();
        let contract = env.register(RecreateDefaultBug, ());
        let mut target = BugTarget {
            contract,
            last_written: None,
            reads_since_write: 0,
        };
        let trace = chaos_drive(&env, &mut target, 7_777, 4, 1_000);
        assert_no_violations(&trace);
    }
}
