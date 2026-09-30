#![no_std]

//! # Soroban Forge — DAO Governance contract
//!
//! A minimal on-chain governance primitive: members create proposals, cast one
//! vote each (`for`/`against`, tallied in governance-token units), and a
//! proposal is finalised once voting ends — passing when it has a strict
//! majority of `for` votes.
//!
//! Lifecycle:
//!
//! ```text
//! propose (voting_ends = now + duration)
//!   --> Active --vote × n--> voting ends
//!   --> execute: for > against ? Succeeded : Defeated
//!   --> execute (on Succeeded): target.execute(action) --> Executed (terminal)
//!   --> cancel (proposer only): Cancelled (terminal)
//! ```
//!
//! When voting ends, calling `execute` finalises the vote tally:
//! - If `for_votes > against_votes`, the proposal becomes `Succeeded`.
//! - Otherwise, the proposal becomes `Defeated`.
//!
//! Once a proposal is `Succeeded`, calling `execute` performs a real
//! cross-contract call (`target.execute(action)`) using `env.try_invoke_contract`.
//! - If the target invocation succeeds, the proposal transitions to `Executed`.
//! - If the target invocation reverts, `ForgeError::ContractInvocationFailed` is
//!   returned and the proposal remains in the `Succeeded` state (not `Executed`),
//!   leaving target state unchanged and allowing execution to be re-attempted.
//!
//! `Defeated`, `Executed`, `Cancelled`, and still-`Active` proposals cannot be
//! executed; a second `execute` on an `Executed` proposal is rejected.
//!
//! ## Proposal dependencies
//!
//! `propose` may store `requires` edges (all must reach `Executed`) and one
//! `conflicts_with` edge (execution blocks this proposal). The first execute
//! after voting can still finalise a passed proposal to `Succeeded`; later
//! dispatch is checked before any target invocation or bond movement. Missing
//! requirements return `DeadlineReached`, executed conflicts return
//! `InvalidInput`, and cancelled/defeated requirements return
//! `ContractInvocationFailed`. A dependent stranded by cancellation remains
//! `Succeeded` with its bond held; there is no automatic cancellation.
//!
//! Proposal-time cycle detection is iterative DFS over `requires` edges. It
//! uses an explicit Soroban `Vec` stack and a visited set, with O(V + E)
//! work over the reachable subgraph and no recursion depth growth. Dependency
//! records are stored on each persistent proposal; no top-level storage key
//! is added. A future `Queued` timelock composes after dependencies resolve,
//! but neither queueing nor time-based gating is implemented here.
//!
//! ## Proposal bonds (token custody)
//!
//! Proposals are not free. Once the bond is configured (see
//! [`DaoGovernance::configure_bond`]), every `propose` pulls `amount` of the
//! configured SEP-41 `token` from the proposer into this contract's custody
//! *before* any state is written. The bond is released exactly once, on the
//! proposal's terminal transition:
//!
//! ```text
//! configure_bond (one-time): token + amount + treasury
//! propose      --transfer--> proposer -> contract   (BondPosted)
//! execute      (Succeeded -> Executed) -> refund    contract -> proposer
//! cancel       (Active -> Cancelled)   -> refund    contract -> proposer
//! execute      (Active -> Defeated)    -> forfeit   contract -> treasury
//! ```
//!
//! - **Refunded** to the proposer: `Executed` (the proposal passed and was
//!   dispatched) and `Cancelled` (the proposer withdrew it).
//! - **Forfeited** to the configured treasury: `Defeated` (the vote failed).
//!
//! Design decisions (both answered in favour of "no new privileged role"):
//! - *Where does a forfeited bond go?* To a **treasury address fixed at
//!   configuration time**. An arbitrary SEP-41 token cannot be burned, and
//!   parking forfeits inside the contract forever would need a privileged
//!   sweep entrypoint (and would keep the contract non-empty after every
//!   proposal is terminal).
//! - *Who sets the bond?* A **one-time, permissionless `configure_bond`**,
//!   mirroring the multi-sig wallet's `initialize`: the deployer calls it in
//!   the deploy transaction and the configuration is immutable afterwards, so
//!   no admin key exists. Per-proposal self-serve bond amounts were rejected:
//!   a proposer who picks their own amount defeats the anti-spam purpose.
//!
//! An unconfigured contract rejects `propose` with
//! [`ForgeError::NotInitialized`] — bond-less proposals are never accepted,
//! so a deployment that forgets to configure is unusable rather than
//! spam-prone.
//!
//! ## Ordering discipline (load-bearing)
//!
//! Every path that moves bonds follows `crates/escrow/src/lib.rs`: the token
//! transfer runs **first**, state is written **after** it succeeds. A failing
//! transfer reverts the whole invocation with storage untouched — including,
//! on `Executed`, the target dispatch — so there is no state/ledger
//! divergence window. Token failures are bucketed into
//! [`ForgeError::TokenTransferFailed`], never forwarded.
//!
//! Double release is structurally impossible: the bond moves in the same
//! frame as the one and only terminal state transition of the proposal, and
//! every terminal transition is state-gated. A failed release surfaces its
//! error with the proposal still in its pre-transition state (retryable).
//!
//! Authorization model:
//! - `configure_bond` is permissionless and callable exactly once.
//! - `propose` requires the proposer; their signature covers the nested
//!   bond pull.
//! - `vote` requires the voter and is one-vote-per-voter per proposal.
//! - `execute` may be called by anyone (permissionless), but only once voting
//!   has ended — including on paths that pay out or forfeit a bond; contract
//!   self-authorization covers the outgoing transfer.
//! - `cancel_proposal` requires the original proposer and freezes a
//!   not-yet-executed proposal; the refund runs in that same call.
//! - `get_proposal` and `get_bond_config` are read-only views.
//!
//! The `Queued` state is reserved for an optional timelock that lands in a
//! follow-up; it is not reachable through the current public interface.
//! `initialize` configures the SEP-41 governance token once. Each accepted
//! vote is weighted by the voter's token balance at vote time; the balance is
//! not snapshotted, so moving tokens between votes can change influence.
//! Proposal bonds are separate custody and never enter the vote tally.

#[cfg(test)]
extern crate std;

use soroban_forge_shared_utils::{bump_entry as shared_bump_entry, ForgeError};
use soroban_sdk::{
    contract, contractclient, contractevent, contractimpl, contracttype, token, Address, Bytes,
    Env, IntoVal, Map, Symbol, Val,
};

/// Public interface for the Soroban Forge DAO governance contract.
#[contractclient(name = "SorobanForgeDaoGovernanceClient")]
pub trait SorobanForgeDaoGovernance {
    /// Configure the SEP-41 governance token for weighted voting.
    ///
    /// Permissionless and one-time; the first caller fixes the token.
    ///
    /// # Errors
    ///
    /// * [`ForgeError::AlreadyInitialized`] — governance token is already set.
    fn initialize(
        env: Env,
        governance_token: Address,
    ) -> Result<(), soroban_forge_shared_utils::ForgeError>;

    /// Configure the proposal bond for the first and only time.
    ///
    /// Records the SEP-41 `token` every `propose` must post, the `amount` of
    /// that token pulled per proposal, and the `treasury` address that
    /// receives bonds forfeited by defeated proposals.
    ///
    /// Permissionless by design, mirroring the multi-sig wallet's
    /// `initialize`: the deployer calls it in the deploy transaction, and
    /// after that the configuration is immutable. No admin role exists —
    /// there is nothing privileged left to call.
    ///
    /// # Errors
    ///
    /// * [`ForgeError::AlreadyInitialized`] — a bond configuration already
    ///   exists.
    /// * [`ForgeError::InvalidInput`] — `amount <= 0` (a zero bond would
    ///   make `propose` free).
    fn configure_bond(
        env: Env,
        token: Address,
        amount: i128,
        treasury: Address,
    ) -> Result<(), soroban_forge_shared_utils::ForgeError>;

    /// Read the bond configuration (read-only view).
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotInitialized`] — no bond has been configured, in
    ///   which case `propose` is rejected too.
    fn get_bond_config(env: Env) -> Result<BondConfig, soroban_forge_shared_utils::ForgeError>;

    /// Create a new proposal with a target contract and encoded action payload.
    ///
    /// `duration` (seconds) defines how long voting stays open. Returns the
    /// stable proposal id.
    ///
    /// Posting the bond is part of creation: `amount` of the configured
    /// token is transferred from the proposer into this contract *before*
    /// any state is written, so a failed transfer leaves no proposal record.
    ///
    /// # Errors
    ///
    /// * [`ForgeError::InvalidInput`] — `duration == 0`.
    /// * [`ForgeError::NotInitialized`] — no bond configuration; free
    ///   proposals are never accepted.
    /// * [`ForgeError::TokenTransferFailed`] — the bond pull failed
    ///   (insufficient proposer balance, deauthorized token, undeployed
    ///   token contract); no proposal, no counter increment.
    /// * [`ForgeError::ArithmeticOverflow`] — the id counter or the
    ///   running bond-custody total would overflow.
    /// * [`ForgeError::NotFound`] — a dependency id does not exist.
    /// * [`ForgeError::InvalidInput`] — a self edge, duplicate edge, consumed
    ///   proposal reference, or dependency cycle is supplied.
    fn propose(
        env: Env,
        proposer: Address,
        target: Address,
        action: Bytes,
        duration: u64,
        requires: soroban_sdk::Vec<u64>,
        conflicts_with: Option<u64>,
    ) -> Result<u64, soroban_forge_shared_utils::ForgeError>;

    /// Read the execution-order and conflict edges for a proposal.
    fn get_dependencies(
        env: Env,
        proposal_id: u64,
    ) -> Result<DependencyView, soroban_forge_shared_utils::ForgeError>;

    /// Cast `voter`'s balance-weighted vote (for/against) on `proposal_id`.
    /// The SEP-41 balance is read at vote time; zero balance is rejected.
    /// One vote per voter regardless of balance.
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotInitialized`] — governance token has not been set.
    /// * [`ForgeError::InvalidInput`] — proposal inactive, duplicate vote, or
    ///   voter has zero governance-token balance.
    /// * [`ForgeError::DeadlineReached`] — voting has closed.
    /// * [`ForgeError::ArithmeticOverflow`] — adding the weight overflows.
    fn vote(
        env: Env,
        proposal_id: u64,
        voter: Address,
        support: bool,
    ) -> Result<(), soroban_forge_shared_utils::ForgeError>;

    /// Finalise a proposal once voting has ended, and execute passed proposals.
    ///
    /// Also the bond-release path: a proposal that becomes `Defeated`
    /// forfeits its bond to the configured treasury, and one that reaches
    /// `Executed` refunds the bond to the proposer — both inside this call,
    /// before the terminal state is written.
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotFound`] — no proposal with this id.
    /// * [`ForgeError::InvalidInput`] — voting has not ended, or the
    ///   proposal is already terminal.
    /// * [`ForgeError::ContractInvocationFailed`] — the target reverted;
    ///   the proposal stays `Succeeded` and the bond stays in custody.
    /// * [`ForgeError::DeadlineReached`] — a required proposal has not yet
    ///   executed; the succeeded proposal and bond remain unchanged.
    /// * [`ForgeError::InvalidInput`] — the conflicting proposal executed.
    /// * [`ForgeError::ContractInvocationFailed`] — a required proposal was
    ///   cancelled or defeated, permanently blocking this proposal.
    /// * [`ForgeError::TokenTransferFailed`] — the bond refund/forfeit
    ///   transfer failed; the proposal state is untouched (still retryable).
    fn execute(env: Env, proposal_id: u64) -> Result<(), soroban_forge_shared_utils::ForgeError>;

    /// Withdraw a proposal that has not yet been executed.
    ///
    /// Only the original proposer may cancel. Once cancelled the proposal is
    /// frozen against further votes and cannot be executed. The bond is
    /// refunded to the proposer in the same call, before the state write.
    ///
    /// * [`ForgeError::NotFound`] — no proposal with this id.
    /// * [`ForgeError::Unauthorized`] — `proposer` is not the original proposer.
    /// * [`ForgeError::InvalidInput`] — the proposal is no longer `Active`.
    /// * [`ForgeError::TokenTransferFailed`] — the bond refund failed; the
    ///   proposal stays `Active` and the bond stays in custody.
    fn cancel_proposal(
        env: Env,
        proposal_id: u64,
        proposer: Address,
    ) -> Result<(), soroban_forge_shared_utils::ForgeError>;

    /// Read a stored proposal by id (read-only view).
    fn get_proposal(
        env: Env,
        proposal_id: u64,
    ) -> Result<Proposal, soroban_forge_shared_utils::ForgeError>;

    /// Return the total number of proposals created (read-only view).
    fn get_proposal_count(env: Env) -> u64;

    /// Read a paginated slice of proposals ordered by proposal ID (read-only view).
    fn get_proposals(
        env: Env,
        offset: u32,
        limit: u32,
    ) -> Result<soroban_sdk::Vec<Proposal>, soroban_forge_shared_utils::ForgeError>;

    /// Check whether a voter has already cast a vote on a proposal (read-only view).
    fn has_voted(
        env: Env,
        proposal_id: u64,
        voter: Address,
    ) -> Result<bool, soroban_forge_shared_utils::ForgeError>;

    /// Permissionless keeper entrypoint: extend the persistent storage TTL of a proposal.
    ///
    /// # Errors
    ///
    /// * [`ForgeError::NotFound`] — no proposal with this id.
    fn touch_ttl(env: Env, proposal_id: u64) -> Result<(), soroban_forge_shared_utils::ForgeError>;

    /// Read how many active proposals `proposer` currently has in flight (read-only view).
    fn get_active_proposal_count(env: Env, proposer: Address) -> u32;
}

/// Lifecycle state of a governance proposal.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProposalState {
    /// Open for voting.
    Active,
    /// Approved and ready for execution.
    Succeeded,
    /// Rejected or expired.
    Defeated,
    /// Successfully executed on-chain (terminal).
    Executed,
    /// Queued for delayed execution (optional timelock).
    Queued,
    /// Withdrawn by the proposer before execution; terminal and immutable.
    Cancelled,
}

/// Lifecycle state of the bond posted for a proposal.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BondState {
    /// Held by this contract since `propose`; the only state a live
    /// proposal can be in.
    Posted,
    /// Returned to the proposer when the proposal reached `Executed` or
    /// `Cancelled` (terminal).
    Refunded,
    /// Sent to the configured treasury when the proposal was `Defeated`
    /// (terminal).
    Forfeited,
}

/// The one-time proposal-bond configuration.
///
/// Written once by [`DaoGovernance::configure_bond`] and immutable
/// afterwards; read by `propose` (what to pull) and by the terminal
/// transitions (where a forfeit goes).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BondConfig {
    /// SEP-41 token every proposal must post as a bond.
    pub token: Address,
    /// Amount of `token` pulled from the proposer on `propose`.
    pub amount: i128,
    /// Receives bonds forfeited by defeated proposals. Never receives
    /// refunds and holds no other authority.
    pub treasury: Address,
}

/// A single governance proposal.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Proposal {
    /// Stable identifier assigned at creation.
    pub proposal_id: u64,
    /// Address that created the proposal.
    pub proposer: Address,
    /// Target contract to invoke on successful execution.
    pub target: Address,
    /// Encoded action to execute on success.
    pub action: Bytes,
    /// Tally of "for" votes (in governance-token units).
    pub for_votes: i128,
    /// Tally of "against" votes (in governance-token units).
    pub against_votes: i128,
    /// Ledger timestamp at which voting closes.
    pub voting_ends: u64,
    /// Current state.
    pub state: ProposalState,
    /// SEP-41 token this proposal's bond was posted in (the configured
    /// token at creation time).
    pub bond_token: Address,
    /// Bond amount posted at creation; the exact figure refunded or
    /// forfeited on the terminal transition.
    pub bond_amount: i128,
    /// Lifecycle of this proposal's bond. Moves in the same frame as the
    /// terminal proposal state, exactly once.
    pub bond_state: BondState,
    /// Proposals that must execute before this one.
    pub requires: soroban_sdk::Vec<u64>,
    /// Proposal whose execution blocks this one.
    pub conflicts_with: Option<u64>,
}

/// Dependency edges exposed to indexers independently of the full proposal.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DependencyView {
    /// Proposals that must execute first.
    pub requires: soroban_sdk::Vec<u64>,
    /// Proposal whose execution blocks this proposal.
    pub conflicts_with: Option<u64>,
}

/// Bump a persistent entry's TTL to the workspace policy's 30-day horizon
/// when it falls inside its one-day threshold — see
/// `soroban_forge_shared_utils::ttl`.
///
/// Thin wrapper over [`soroban_forge_shared_utils::bump_entry`] — the
/// canonical helper (issue #127); the policy lives there.
fn bump_entry(env: &Env, key: &DataKey) {
    shared_bump_entry(env, key);
}

/// Instance and persistent storage keys.
#[contracttype]
enum DataKey {
    /// The proposal record for `u64` id (persistent storage).
    Proposal(u64),
    /// Marks that `voter` has already voted on `proposal_id`.
    Vote(u64, Address),
    /// Monotonic proposal id counter.
    Count,
    /// The immutable [`BondConfig`]; present only once configured.
    Bond,
    /// Running total of bonds currently in custody (`i128`). Starts at 0,
    /// grows by `bond_amount` on every `propose`, shrinks on every refund
    /// or forfeit; must always equal this contract's balance of the
    /// configured token while proposals are live.
    BondHeld,
    /// Immutable SEP-41 governance token used to weight votes (instance storage).
    GovernanceToken,
    /// Number of concurrent active proposals currently in flight for a proposer.
    ActiveProposalCount(Address),
}

/// Maximum number of concurrent active proposals a single proposer may have by default.
pub const DEFAULT_MAX_ACTIVE_PROPOSALS: u32 = 5;

/// The deployable DAO governance contract.
#[contract]
pub struct DaoGovernance;

#[contractimpl]
impl DaoGovernance {
    /// Configure the governance token exactly once.
    pub fn initialize(env: Env, governance_token: Address) -> Result<(), ForgeError> {
        let storage = env.storage().instance();
        if storage.has(&DataKey::GovernanceToken) {
            return Err(ForgeError::AlreadyInitialized);
        }
        storage.set(&DataKey::GovernanceToken, &governance_token);
        Ok(())
    }

    /// Configure the proposal bond for the first and only time.
    ///
    /// Permissionless one-shot (see [`SorobanForgeDaoGovernance::configure_bond`]):
    /// the deployer calls it in the deploy transaction and the configuration
    /// never changes afterwards, so no privileged role exists.
    pub fn configure_bond(
        env: Env,
        token: Address,
        amount: i128,
        treasury: Address,
    ) -> Result<(), ForgeError> {
        // One-shot check first: a second call is a programming error even if
        // its arguments are also invalid.
        if env.storage().instance().has(&DataKey::Bond) {
            return Err(ForgeError::AlreadyInitialized);
        }
        // A zero or negative bond would make `propose` free (or pay the
        // proposer) — the exact spam vector this feature exists to close.
        if amount <= 0 {
            return Err(ForgeError::InvalidInput);
        }
        env.storage().instance().set(
            &DataKey::Bond,
            &BondConfig {
                token,
                amount,
                treasury,
            },
        );
        env.storage().instance().set(&DataKey::BondHeld, &0i128);
        Ok(())
    }

    /// Read the bond configuration (read-only view).
    ///
    /// Returns `ForgeError::NotInitialized` while the contract has no bond
    /// configuration — the same precondition `propose` enforces.
    pub fn get_bond_config(env: Env) -> Result<BondConfig, ForgeError> {
        Self::bond_config_impl(&env)
    }

    /// Create a new proposal and return its stable id.
    ///
    /// Requires `duration > 0` and a configured bond. The proposer is
    /// authorized at creation time, and their authorization covers the
    /// nested bond pull.
    ///
    /// Ordering (load-bearing): the bond transfer runs **before** the id
    /// counter, the proposal record, and the custody total are written, so a
    /// failed transfer leaves no proposal record — see the module docs.
    pub fn propose(
        env: Env,
        proposer: Address,
        target: Address,
        action: Bytes,
        duration: u64,
        requires: soroban_sdk::Vec<u64>,
        conflicts_with: Option<u64>,
    ) -> Result<u64, ForgeError> {
        if duration == 0 {
            return Err(ForgeError::InvalidInput);
        }
        proposer.require_auth();
        let active_count = Self::active_proposal_count(&env, &proposer);
        if active_count >= DEFAULT_MAX_ACTIVE_PROPOSALS {
            return Err(ForgeError::ProposerCooldown);
        }
        let bond = Self::bond_config_impl(&env)?;

        // Pure validation first: id, deadline, and custody arithmetic are
        // all checked before a single token moves or a single key is
        // written. `BondHeld` overflow is the i128 boundary guard on the
        // running custody total.
        let count: u64 = env.storage().instance().get(&DataKey::Count).unwrap_or(0);
        let proposal_id = count.checked_add(1).ok_or(ForgeError::ArithmeticOverflow)?;
        Self::validate_dependencies(&env, proposal_id, &requires, conflicts_with)?;
        let voting_ends = env
            .ledger()
            .timestamp()
            .checked_add(duration)
            .ok_or(ForgeError::ArithmeticOverflow)?;
        let held = Self::bond_held(&env);
        let next_held = held
            .checked_add(bond.amount)
            .ok_or(ForgeError::ArithmeticOverflow)?;

        // Pull the bond before writing any state. If the proposer lacks
        // balance or the token misbehaves, the invocation reverts here with
        // storage untouched and the counter unmoved.
        transfer_to_contract(&env, &bond.token, &proposer, bond.amount)?;

        let proposal = Proposal {
            proposal_id,
            proposer: proposer.clone(),
            target,
            action,
            for_votes: 0,
            against_votes: 0,
            voting_ends,
            state: ProposalState::Active,
            bond_token: bond.token.clone(),
            bond_amount: bond.amount,
            bond_state: BondState::Posted,
            requires,
            conflicts_with,
        };
        let key = DataKey::Proposal(proposal_id);
        env.storage().instance().set(&DataKey::Count, &proposal_id);
        env.storage().persistent().set(&key, &proposal);
        bump_entry(&env, &key);
        env.storage().instance().set(&DataKey::BondHeld, &next_held);
        Self::inc_active_proposals(&env, &proposer)?;
        events::proposed(&env, &proposal);
        events::bond_posted(&env, proposal_id, &bond.token, bond.amount);
        Ok(proposal_id)
    }

    fn validate_dependencies(
        env: &Env,
        proposal_id: u64,
        requires: &soroban_sdk::Vec<u64>,
        conflicts_with: Option<u64>,
    ) -> Result<(), ForgeError> {
        let mut seen = Map::<u64, bool>::new(env);
        for id in requires.iter().chain(conflicts_with) {
            if id == proposal_id || seen.contains_key(id) {
                return Err(ForgeError::InvalidInput);
            }
            seen.set(id, true);
            let dependency = Self::get_proposal_impl(env, id)?;
            if matches!(
                dependency.state,
                ProposalState::Executed | ProposalState::Defeated | ProposalState::Cancelled
            ) {
                return Err(ForgeError::InvalidInput);
            }
        }
        // Iterative DFS with an explicit stack avoids recursive Soroban stack growth.
        let mut stack = requires.clone();
        let mut visited = Map::<u64, bool>::new(env);
        while let Some(id) = stack.pop_back() {
            if id == proposal_id {
                return Err(ForgeError::InvalidInput);
            }
            if visited.contains_key(id) {
                continue;
            }
            visited.set(id, true);
            for parent in Self::get_proposal_impl(env, id)?.requires.iter() {
                stack.push_back(parent);
            }
        }
        Ok(())
    }

    fn check_dependencies(env: &Env, proposal: &Proposal) -> Result<(), ForgeError> {
        for id in proposal.requires.iter() {
            match Self::get_proposal_impl(env, id)?.state {
                ProposalState::Executed => {}
                ProposalState::Cancelled | ProposalState::Defeated => {
                    return Err(ForgeError::ContractInvocationFailed)
                }
                _ => return Err(ForgeError::DeadlineReached),
            }
        }
        if let Some(id) = proposal.conflicts_with {
            if Self::get_proposal_impl(env, id)?.state == ProposalState::Executed {
                return Err(ForgeError::InvalidInput);
            }
        }
        Ok(())
    }

    /// Cast a vote on an active proposal.
    ///
    /// Requires the voter. Each voter may vote exactly once; voting is closed
    /// once the deadline (`voting_ends`) passes.
    pub fn vote(
        env: Env,
        proposal_id: u64,
        voter: Address,
        support: bool,
    ) -> Result<(), ForgeError> {
        let governance_token: Address = env
            .storage()
            .instance()
            .get(&DataKey::GovernanceToken)
            .ok_or(ForgeError::NotInitialized)?;
        let mut proposal = Self::get_proposal_impl(&env, proposal_id)?;
        if proposal.state != ProposalState::Active {
            return Err(ForgeError::InvalidInput);
        }
        if env.ledger().timestamp() >= proposal.voting_ends {
            return Err(ForgeError::DeadlineReached);
        }
        voter.require_auth();

        let vote_key = DataKey::Vote(proposal_id, voter.clone());
        if env.storage().instance().has(&vote_key) {
            return Err(ForgeError::InvalidInput);
        }

        // soroban-sdk 27.0.6's generated SEP-41 token client exposes
        // `balance(Address) -> i128` (the same interface used by escrow and
        // the other token-integrated contracts).
        let weight = token::TokenClient::new(&env, &governance_token).balance(&voter);
        if weight == 0 {
            return Err(ForgeError::InvalidInput);
        }

        if support {
            proposal.for_votes = proposal
                .for_votes
                .checked_add(weight)
                .ok_or(ForgeError::ArithmeticOverflow)?;
        } else {
            proposal.against_votes = proposal
                .against_votes
                .checked_add(weight)
                .ok_or(ForgeError::ArithmeticOverflow)?;
        }
        let key = DataKey::Proposal(proposal_id);
        env.storage().instance().set(&vote_key, &true);
        env.storage().persistent().set(&key, &proposal);
        bump_entry(&env, &key);
        events::vote_cast(&env, proposal_id, &voter, support, weight);
        Ok(())
    }

    /// Finalise a proposal once voting has ended, and execute passed proposals.
    ///
    /// Callable by anyone after the deadline (permissionless execution), and
    /// — on the paths that release a bond — permissionless in the token
    /// sense too: contract self-authorization covers the outgoing transfer.
    ///
    /// - An `Active` proposal past deadline transitions to `Succeeded` on a
    ///   strict majority of `for` votes (the bond stays in custody until a
    ///   terminal transition), or to `Defeated` otherwise — forfeiting the
    ///   bond to the treasury **before** the state write.
    /// - A `Succeeded` proposal performs a real cross-contract call to `target`
    ///   with `action` (`target.execute(action)`). On successful invocation,
    ///   it refunds the bond to the proposer, then transitions to the
    ///   terminal `Executed` state and emits an `Executed` event.
    /// - If the target invocation reverts, `ForgeError::ContractInvocationFailed`
    ///   is returned and the proposal remains `Succeeded` (not `Executed`),
    ///   leaving target state unchanged and the bond in custody.
    /// - If a bond refund/forfeit transfer fails,
    ///   `ForgeError::TokenTransferFailed` is returned and the proposal
    ///   state is untouched (still retryable).
    /// - `Defeated`, `Executed`, `Cancelled`, and still-`Active` proposals
    ///   cannot be executed.
    pub fn execute(env: Env, proposal_id: u64) -> Result<(), ForgeError> {
        let mut proposal = Self::get_proposal_impl(&env, proposal_id)?;
        if env.ledger().timestamp() < proposal.voting_ends {
            return Err(ForgeError::InvalidInput);
        }

        match proposal.state {
            ProposalState::Active => {
                let key = DataKey::Proposal(proposal_id);
                if proposal.for_votes > proposal.against_votes {
                    proposal.state = ProposalState::Succeeded;
                    env.storage().persistent().set(&key, &proposal);
                    bump_entry(&env, &key);
                    events::finalised(
                        &env,
                        proposal_id,
                        proposal.state.clone(),
                        proposal.for_votes,
                        proposal.against_votes,
                    );
                    Ok(())
                } else {
                    // Defeat: forfeit the bond to the treasury. Custody
                    // arithmetic and the transfer both run before the state
                    // write, so a failure leaves the proposal `Active`.
                    let bond = Self::bond_config_impl(&env)?;
                    let next_held = Self::bond_held(&env)
                        .checked_sub(proposal.bond_amount)
                        .ok_or(ForgeError::ArithmeticOverflow)?;
                    transfer_from_contract(
                        &env,
                        &bond.token,
                        &bond.treasury,
                        proposal.bond_amount,
                    )?;

                    proposal.state = ProposalState::Defeated;
                    proposal.bond_state = BondState::Forfeited;
                    env.storage().persistent().set(&key, &proposal);
                    bump_entry(&env, &key);
                    env.storage().instance().set(&DataKey::BondHeld, &next_held);
                    Self::dec_active_proposals(&env, &proposal.proposer);
                    events::finalised(
                        &env,
                        proposal_id,
                        proposal.state.clone(),
                        proposal.for_votes,
                        proposal.against_votes,
                    );
                    events::bond_released(
                        &env,
                        proposal_id,
                        &bond.token,
                        proposal.bond_amount,
                        &bond.treasury,
                        true,
                    );
                    Ok(())
                }
            }
            ProposalState::Succeeded => {
                // Finalisation is independent from dispatch ordering: a
                // passed dependent may become Succeeded while it waits, but
                // the target call and bond release stay gated here.
                Self::check_dependencies(&env, &proposal)?;
                let target = &proposal.target;
                let payload_val: Val = proposal.action.clone().into_val(&env);
                let args = soroban_sdk::vec![&env, payload_val];
                let result = env.try_invoke_contract::<(), ForgeError>(
                    target,
                    &Symbol::new(&env, "execute"),
                    args,
                );

                if let Err(_) | Ok(Err(_)) = result {
                    return Err(ForgeError::ContractInvocationFailed);
                }

                // Refund the bond before the terminal state write, so a
                // failed payout reverts the whole invocation — including
                // the target dispatch above — and leaves the proposal
                // retryable in `Succeeded`.
                let bond = Self::bond_config_impl(&env)?;
                let next_held = Self::bond_held(&env)
                    .checked_sub(proposal.bond_amount)
                    .ok_or(ForgeError::ArithmeticOverflow)?;
                transfer_from_contract(
                    &env,
                    &bond.token,
                    &proposal.proposer,
                    proposal.bond_amount,
                )?;

                proposal.state = ProposalState::Executed;
                proposal.bond_state = BondState::Refunded;
                let key = DataKey::Proposal(proposal_id);
                env.storage().persistent().set(&key, &proposal);
                bump_entry(&env, &key);
                env.storage().instance().set(&DataKey::BondHeld, &next_held);
                Self::dec_active_proposals(&env, &proposal.proposer);
                events::finalised(
                    &env,
                    proposal_id,
                    proposal.state.clone(),
                    proposal.for_votes,
                    proposal.against_votes,
                );
                events::bond_released(
                    &env,
                    proposal_id,
                    &bond.token,
                    proposal.bond_amount,
                    &proposal.proposer,
                    false,
                );
                Ok(())
            }
            ProposalState::Defeated
            | ProposalState::Executed
            | ProposalState::Cancelled
            | ProposalState::Queued => Err(ForgeError::InvalidInput),
        }
    }

    /// Withdraw an active proposal before it is executed.
    ///
    /// Requires the original proposer. A proposal may be cancelled even after
    /// voting ends and quorum is met, as long as it has not been executed (or
    /// already cancelled). A cancelled proposal is terminal: further votes and
    /// execution are rejected.
    ///
    /// The bond is refunded to the proposer in this same call: transfer
    /// first, then the `Cancelled` state write — a failed refund surfaces
    /// `TokenTransferFailed` with the proposal still `Active`.
    ///
    /// * [`ForgeError::NotFound`] — no proposal with this id.
    /// * [`ForgeError::Unauthorized`] — `proposer` is not the original proposer.
    /// * [`ForgeError::InvalidInput`] — the proposal is no longer `Active`.
    /// * [`ForgeError::TokenTransferFailed`] — the bond refund failed.
    pub fn cancel_proposal(
        env: Env,
        proposal_id: u64,
        proposer: Address,
    ) -> Result<(), ForgeError> {
        let mut proposal = Self::get_proposal_impl(&env, proposal_id)?;
        if proposal.state != ProposalState::Active {
            return Err(ForgeError::InvalidInput);
        }
        if proposer != proposal.proposer {
            return Err(ForgeError::Unauthorized);
        }
        proposer.require_auth();

        // Refund before the terminal state write, mirroring `execute`.
        let bond = Self::bond_config_impl(&env)?;
        let next_held = Self::bond_held(&env)
            .checked_sub(proposal.bond_amount)
            .ok_or(ForgeError::ArithmeticOverflow)?;
        transfer_from_contract(&env, &bond.token, &proposal.proposer, proposal.bond_amount)?;

        proposal.state = ProposalState::Cancelled;
        proposal.bond_state = BondState::Refunded;
        let key = DataKey::Proposal(proposal_id);
        env.storage().persistent().set(&key, &proposal);
        bump_entry(&env, &key);
        env.storage().instance().set(&DataKey::BondHeld, &next_held);
        Self::dec_active_proposals(&env, &proposal.proposer);
        events::bond_released(
            &env,
            proposal_id,
            &bond.token,
            proposal.bond_amount,
            &proposal.proposer,
            false,
        );
        Ok(())
    }

    /// Read a stored proposal by id (read-only view).
    pub fn get_proposal(env: Env, proposal_id: u64) -> Result<Proposal, ForgeError> {
        Self::get_proposal_impl(&env, proposal_id)
    }

    /// Return a proposal's dependency edges for indexers.
    pub fn get_dependencies(env: Env, proposal_id: u64) -> Result<DependencyView, ForgeError> {
        let proposal = Self::get_proposal_impl(&env, proposal_id)?;
        Ok(DependencyView {
            requires: proposal.requires,
            conflicts_with: proposal.conflicts_with,
        })
    }

    /// Return the total number of proposals created (read-only view).
    pub fn get_proposal_count(env: Env) -> u64 {
        env.storage().instance().get(&DataKey::Count).unwrap_or(0)
    }

    /// Read a paginated slice of proposals ordered by proposal ID (read-only view).
    ///
    /// Bounds clamping:
    /// - `limit == 0` returns `ForgeError::InvalidInput`.
    /// - If `offset >= total`, returns an empty `Vec`.
    /// - Returns at most `limit` items without overflowing.
    pub fn get_proposals(
        env: Env,
        offset: u32,
        limit: u32,
    ) -> Result<soroban_sdk::Vec<Proposal>, ForgeError> {
        if limit == 0 {
            return Err(ForgeError::InvalidInput);
        }

        let total: u64 = env.storage().instance().get(&DataKey::Count).unwrap_or(0);
        let offset_u64 = u64::from(offset);
        if offset_u64 >= total {
            return Ok(soroban_sdk::Vec::new(&env));
        }

        let start = offset_u64
            .checked_add(1)
            .ok_or(ForgeError::ArithmeticOverflow)?;
        let limit_u64 = u64::from(limit);
        let end = total.min(offset_u64.saturating_add(limit_u64));

        let mut proposals = soroban_sdk::Vec::new(&env);
        for id in start..=end {
            let proposal = Self::get_proposal_impl(&env, id)?;
            proposals.push_back(proposal);
        }

        Ok(proposals)
    }

    /// Check whether `voter` has voted on `proposal_id` (read-only view).
    ///
    /// Returns `ForgeError::NotFound` if `proposal_id` does not exist.
    pub fn has_voted(env: Env, proposal_id: u64, voter: Address) -> Result<bool, ForgeError> {
        // Verify proposal existence
        Self::get_proposal_impl(&env, proposal_id)?;

        let vote_key = DataKey::Vote(proposal_id, voter);
        Ok(env.storage().instance().has(&vote_key))
    }

    /// Load the bond configuration, or `NotInitialized` if the contract was
    /// never configured (internal, reference-taking form of
    /// [`Self::get_bond_config`]).
    fn bond_config_impl(env: &Env) -> Result<BondConfig, ForgeError> {
        env.storage()
            .instance()
            .get(&DataKey::Bond)
            .ok_or(ForgeError::NotInitialized)
    }

    /// The running total of bonds currently in custody. Absent only if the
    /// configuration itself is absent (then `bond_config_impl` fails first).
    fn bond_held(env: &Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::BondHeld)
            .unwrap_or(0)
    }

    fn get_proposal_impl(env: &Env, proposal_id: u64) -> Result<Proposal, ForgeError> {
        env.storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(ForgeError::NotFound)
    }

    /// Permissionless keeper: bump the proposal entry's TTL without changing
    /// any state. The existence check is deliberate — touching a missing
    /// id must fail loudly with `ForgeError::NotFound`.
    pub fn touch_ttl(env: Env, proposal_id: u64) -> Result<(), ForgeError> {
        let key = DataKey::Proposal(proposal_id);
        if !env.storage().persistent().has(&key) {
            return Err(ForgeError::NotFound);
        }
        bump_entry(&env, &key);
        Ok(())
    }

    /// Read how many active proposals `proposer` currently has in flight (read-only view).
    pub fn get_active_proposal_count(env: Env, proposer: Address) -> u32 {
        Self::active_proposal_count(&env, &proposer)
    }

    fn active_proposal_count(env: &Env, proposer: &Address) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::ActiveProposalCount(proposer.clone()))
            .unwrap_or(0)
    }

    fn inc_active_proposals(env: &Env, proposer: &Address) -> Result<(), ForgeError> {
        let current = Self::active_proposal_count(env, proposer);
        let next = current
            .checked_add(1)
            .ok_or(ForgeError::ArithmeticOverflow)?;
        env.storage()
            .instance()
            .set(&DataKey::ActiveProposalCount(proposer.clone()), &next);
        Ok(())
    }

    fn dec_active_proposals(env: &Env, proposer: &Address) {
        let current = Self::active_proposal_count(env, proposer);
        let next = current.saturating_sub(1);
        if next == 0 {
            env.storage()
                .instance()
                .remove(&DataKey::ActiveProposalCount(proposer.clone()));
        } else {
            env.storage()
                .instance()
                .set(&DataKey::ActiveProposalCount(proposer.clone()), &next);
        }
    }
}

/// Move `amount` of `token` from `from` into this contract.
///
/// The proposer's `require_auth` on `propose` covers the nested token
/// authorization; no separate allowance is needed for a `transfer` pull when
/// the holder authorizes the invocation.
///
/// Token failures are bucketed into [`ForgeError::TokenTransferFailed`]
/// rather than forwarded: a client receiving `Error(Contract, #N)` cannot
/// know whether `N` came from the token or this contract, and forwarding the
/// raw discriminant invites silent misinterpretation. The root cause remains
/// visible in the transaction's diagnostic events. Copied from
/// `crates/escrow/src/lib.rs::transfer_to_contract`.
fn transfer_to_contract(
    env: &Env,
    token: &Address,
    from: &Address,
    amount: i128,
) -> Result<(), ForgeError> {
    match token::TokenClient::new(env, token).try_transfer(
        from,
        env.current_contract_address(),
        &amount,
    ) {
        Ok(Ok(())) => Ok(()),
        // Token returned a typed error (insufficient balance, missing
        // trustline, custom token logic) or the host aborted (most
        // commonly an undeployed token address). The raw discriminant is
        // intentionally discarded — see the bucketing note above.
        _ => Err(ForgeError::TokenTransferFailed),
    }
}

/// Move `amount` of `token` from this contract to `to` (bond refund or
/// forfeit). Same bucketing rule as [`transfer_to_contract`].
fn transfer_from_contract(
    env: &Env,
    token: &Address,
    to: &Address,
    amount: i128,
) -> Result<(), ForgeError> {
    match token::TokenClient::new(env, token).try_transfer(
        &env.current_contract_address(),
        to,
        &amount,
    ) {
        Ok(Ok(())) => Ok(()),
        _ => Err(ForgeError::TokenTransferFailed),
    }
}

/// Lifecycle events emitted by the DAO governance contract.
mod events {
    use super::*;

    #[contractevent]
    pub struct Proposed {
        #[topic]
        pub proposal_id: u64,
        pub data: Proposal,
    }

    #[contractevent]
    pub struct VoteCast {
        #[topic]
        pub proposal_id: u64,
        pub voter: Address,
        pub support: bool,
        pub weight: i128,
    }

    #[contractevent]
    pub struct Finalised {
        #[topic]
        pub proposal_id: u64,
        pub state: ProposalState,
        pub for_votes: i128,
        pub against_votes: i128,
    }

    /// A bond entered custody with the proposal's creation.
    #[contractevent]
    pub struct BondPosted {
        #[topic]
        pub proposal_id: u64,
        pub token: Address,
        pub amount: i128,
    }

    /// A bond left custody on a terminal transition: back to the proposer
    /// (`forfeited == false`) or to the treasury (`forfeited == true`).
    #[contractevent]
    pub struct BondReleased {
        #[topic]
        pub proposal_id: u64,
        pub token: Address,
        pub amount: i128,
        pub to: Address,
        pub forfeited: bool,
    }

    pub fn proposed(env: &Env, proposal: &Proposal) {
        Proposed {
            proposal_id: proposal.proposal_id,
            data: proposal.clone(),
        }
        .publish(env);
    }

    pub fn vote_cast(env: &Env, proposal_id: u64, voter: &Address, support: bool, weight: i128) {
        VoteCast {
            proposal_id,
            voter: voter.clone(),
            support,
            weight,
        }
        .publish(env);
    }

    pub fn finalised(
        env: &Env,
        proposal_id: u64,
        state: ProposalState,
        for_votes: i128,
        against_votes: i128,
    ) {
        Finalised {
            proposal_id,
            state,
            for_votes,
            against_votes,
        }
        .publish(env);
    }

    pub fn bond_posted(env: &Env, proposal_id: u64, token: &Address, amount: i128) {
        BondPosted {
            proposal_id,
            token: token.clone(),
            amount,
        }
        .publish(env);
    }

    pub fn bond_released(
        env: &Env,
        proposal_id: u64,
        token: &Address,
        amount: i128,
        to: &Address,
        forfeited: bool,
    ) {
        BondReleased {
            proposal_id,
            token: token.clone(),
            amount,
            to: to.clone(),
            forfeited,
        }
        .publish(env);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_forge_test_utils::{MockTarget, MockTargetClient, RevertingTarget, TestAccounts};
    use soroban_sdk::testutils::{Address as _, Events as _, Ledger as _};
    use soroban_sdk::token::{Client as TokenClient, StellarAssetClient};
    use soroban_sdk::{Bytes, Env};

    const START: u64 = 1_000_000;
    const DURATION: u64 = 86_400;
    /// The bond amount every fixture configures.
    const BOND: i128 = 100;
    /// Starting balance minted to each account that proposes in a test.
    const FUNDS: i128 = 10_000;

    /// Fresh env with a real Stellar Asset Contract and a configured bond:
    /// mocked auths, the DAO contract, a `BOND` bond (treasury =
    /// `accounts.deployer`), and four funded proposer accounts. No target
    /// and no proposal — the test registers its own.
    ///
    /// Returns `(env, token, token_client, contract_id, client, accounts)`.
    macro_rules! bonded_env {
        () => {{
            let env = Env::default();
            env.mock_all_auths();
            env.ledger().set_timestamp(START);

            let admin = Address::generate(&env);
            let sac = env.register_stellar_asset_contract_v2(admin);
            let token = sac.address();
            let token_admin = StellarAssetClient::new(&env, &token);
            let token_client = TokenClient::new(&env, &token);

            let contract_id = env.register(DaoGovernance, ());
            let client = SorobanForgeDaoGovernanceClient::new(&env, &contract_id);
            let accounts = TestAccounts::generate(&env);
            client.initialize(&token);
            client.configure_bond(&token, &BOND, &accounts.deployer);
            for who in [
                &accounts.user1,
                &accounts.user2,
                &accounts.user3,
                &accounts.validator,
            ] {
                token_admin.mint(who, &FUNDS);
            }
            (env, token, token_client, contract_id, client, accounts)
        }};
    }

    /// [`bonded_env!`] plus a registered mock target.
    ///
    /// Returns `(env, token, token_client, contract_id, client, accounts,
    /// target_id)`.
    macro_rules! fresh_bond {
        () => {{
            let (env, token, token_client, contract_id, client, accounts) = bonded_env!();
            let target_id = env.register(MockTarget, ());
            (
                env,
                token,
                token_client,
                contract_id,
                client,
                accounts,
                target_id,
            )
        }};
    }

    /// The default fixture: [`fresh_bond!`] plus a pending proposal from
    /// `user1`. Returns `(env, client, accounts, proposal_id, target_id)` —
    /// the shape the lifecycle suite was written against; token handles are
    /// available from [`fresh_bond!`] for custody assertions.
    macro_rules! setup {
        () => {{
            let (env, _token, _token_client, _contract_id, client, accounts, target_id) =
                fresh_bond!();
            let proposal_id = client.propose(
                &accounts.user1,
                &target_id,
                &payload(&env),
                &DURATION,
                &soroban_sdk::Vec::new(&env),
                &None,
            );
            (env, client, accounts, proposal_id, target_id)
        }};
    }

    /// A registered contract with **no** bond configuration, for the
    /// configuration entrypoint and its rejection paths.
    ///
    /// Returns `(env, client, accounts, target_id)`.
    macro_rules! unbonded {
        () => {{
            let env = Env::default();
            env.mock_all_auths();
            env.ledger().set_timestamp(START);
            let contract_id = env.register(DaoGovernance, ());
            let client = SorobanForgeDaoGovernanceClient::new(&env, &contract_id);
            let accounts = TestAccounts::generate(&env);
            let target_id = env.register(MockTarget, ());
            (env, client, accounts, target_id)
        }};
    }

    fn payload(env: &Env) -> Bytes {
        Bytes::from_array(env, &[0xC0, 0xDE, 0x00, 0xFF])
    }

    /// Read the contract's internal custody total (in-crate access to the
    /// instance key; the public invariant check is the token balance).
    fn bond_held(env: &Env, contract_id: &Address) -> i128 {
        env.as_contract(contract_id, || {
            env.storage()
                .instance()
                .get(&DataKey::BondHeld)
                .unwrap_or(0)
        })
    }

    /// Overwrite the custody total so a test can reach an i128 boundary the
    /// happy path cannot (used by the arithmetic-boundary tests).
    fn set_bond_held(env: &Env, contract_id: &Address, value: i128) {
        env.as_contract(contract_id, || {
            env.storage().instance().set(&DataKey::BondHeld, &value);
        });
    }

    /// Overwrite a proposal's recorded `bond_amount` so a test can force a
    /// refund to exceed custody (used by the failed-refund test).
    fn tamper_bond_amount(env: &Env, contract_id: &Address, proposal_id: u64, amount: i128) {
        env.as_contract(contract_id, || {
            let mut proposal: Proposal = env
                .storage()
                .persistent()
                .get(&DataKey::Proposal(proposal_id))
                .expect("proposal exists");
            proposal.bond_amount = amount;
            env.storage()
                .persistent()
                .set(&DataKey::Proposal(proposal_id), &proposal);
        });
    }

    /// The contract's own events from the most recent invocation, with the
    /// bond token's `transfer` events filtered out (the SAC emits its own).
    fn dao_events(
        env: &Env,
        contract_id: &Address,
    ) -> std::vec::Vec<soroban_sdk::xdr::ContractEvent> {
        env.events()
            .all()
            .filter_by_contract(contract_id)
            .events()
            .to_vec()
    }

    /// The most recent invocation must have moved `amount` of the bond token
    /// from `from` to `to`: the SAC's own `transfer` event is the on-chain
    /// evidence that custody actually changed hands.
    fn assert_transfer_event(env: &Env, from: &Address, to: &Address, amount: i128) {
        use soroban_sdk::xdr::{ContractEventBody, ScVal};

        let all = env.events().all();
        let events = all.events();
        let transfer_topic = ScVal::Symbol("transfer".try_into().unwrap());
        let transfers: std::vec::Vec<_> = events
            .iter()
            .filter(|event| {
                matches!(
                    &event.body,
                    ContractEventBody::V0(body) if body.topics.first() == Some(&transfer_topic)
                )
            })
            .collect();
        assert_eq!(
            transfers.len(),
            1,
            "expected exactly one SAC transfer event"
        );
        let event = transfers[0];
        let ContractEventBody::V0(body) = &event.body;
        assert_eq!(body.topics.get(1).unwrap(), &ScVal::from(from));
        assert_eq!(body.topics.get(2).unwrap(), &ScVal::from(to));
        assert_eq!(
            body.data,
            ScVal::I128(soroban_sdk::xdr::Int128Parts {
                hi: (amount >> 64) as i64,
                lo: amount as u64,
            })
        );
    }

    /// The contract's `bond_released` event from the most recent invocation
    /// must be keyed by `proposal_id` and carry the full settlement payload:
    /// token, amount, recipient and the forfeited flag (an ScMap data field).
    #[allow(clippy::too_many_arguments)]
    fn assert_bond_released(
        env: &Env,
        contract_id: &Address,
        proposal_id: u64,
        token: &Address,
        amount: i128,
        to: &Address,
        forfeited: bool,
    ) {
        use soroban_sdk::xdr::{ContractEventBody, ScVal};

        let released = ScVal::Symbol("bond_released".try_into().unwrap());
        let id = ScVal::U64(proposal_id);
        let events = dao_events(env, contract_id);
        let found: std::vec::Vec<_> = events
            .iter()
            .filter(|event| {
                matches!(
                    &event.body,
                    ContractEventBody::V0(body)
                        if body.topics.first() == Some(&released)
                            && body.topics.get(1) == Some(&id)
                )
            })
            .collect();
        assert_eq!(
            found.len(),
            1,
            "one bond_released event for proposal {proposal_id}"
        );
        let event = found[0];
        let ContractEventBody::V0(body) = &event.body;
        let ScVal::Map(Some(map)) = &body.data else {
            panic!("bond_released data is an ScMap");
        };
        let get = |key: &str| {
            map.iter()
                .find(|entry| entry.key == ScVal::Symbol(key.try_into().unwrap()))
                .map(|entry| entry.val.clone())
                .unwrap_or_else(|| panic!("missing {key}"))
        };
        assert_eq!(get("token"), ScVal::from(token));
        assert_eq!(
            get("amount"),
            ScVal::I128(soroban_sdk::xdr::Int128Parts {
                hi: (amount >> 64) as i64,
                lo: amount as u64,
            })
        );
        assert_eq!(get("to"), ScVal::from(to));
        assert_eq!(get("forfeited"), ScVal::Bool(forfeited));
    }

    #[contract]
    pub struct AuthCheckingTarget;

    #[contractimpl]
    impl AuthCheckingTarget {
        pub fn execute(env: Env, _payload: Bytes) {
            // A target's own authorization requirement is not implicitly
            // satisfied by the DAO's cross-contract invocation.
            let caller = env.current_contract_address();
            caller.require_auth();
        }
    }

    #[test]
    fn propose_succeeds_and_is_active() {
        let (env, client, accounts, proposal_id, target_id) = setup!();
        let proposal = client.get_proposal(&proposal_id);
        assert_eq!(proposal.proposer, accounts.user1);
        assert_eq!(proposal.target, target_id);
        assert_eq!(proposal.action, payload(&env));
        assert_eq!(proposal.state, ProposalState::Active);
        assert_eq!(proposal.for_votes, 0);
        assert_eq!(proposal.against_votes, 0);
    }

    #[test]
    fn propose_assigns_distinct_ids() {
        let (env, client, accounts, _id, target_id) = setup!();
        let id2 = client.propose(
            &accounts.user2,
            &target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        let id3 = client.propose(
            &accounts.user3,
            &target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        assert_ne!(id2, id3);
    }

    #[test]
    fn propose_rejects_zero_duration() {
        let (env, client, accounts, _id, target_id) = setup!();
        let err = client
            .try_propose(
                &accounts.user1,
                &target_id,
                &payload(&env),
                &0_u64,
                &soroban_sdk::Vec::new(&env),
                &None,
            )
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn propose_rejects_self_unknown_and_consumed_dependencies() {
        let (env, client, accounts, first, target) = setup!();
        let self_ref = soroban_sdk::vec![&env, first + 1];
        assert_eq!(
            client
                .try_propose(
                    &accounts.user2,
                    &target,
                    &payload(&env),
                    &DURATION,
                    &self_ref,
                    &None
                )
                .unwrap_err()
                .unwrap(),
            ForgeError::InvalidInput
        );
        let unknown = soroban_sdk::vec![&env, first + 100];
        assert_eq!(
            client
                .try_propose(
                    &accounts.user2,
                    &target,
                    &payload(&env),
                    &DURATION,
                    &unknown,
                    &None
                )
                .unwrap_err()
                .unwrap(),
            ForgeError::NotFound
        );
        client.cancel_proposal(&first, &accounts.user1);
        let consumed = soroban_sdk::vec![&env, first];
        assert_eq!(
            client
                .try_propose(
                    &accounts.user2,
                    &target,
                    &payload(&env),
                    &DURATION,
                    &consumed,
                    &None
                )
                .unwrap_err()
                .unwrap(),
            ForgeError::InvalidInput
        );

        let executed = client.propose(
            &accounts.user2,
            &target,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        client.vote(&executed, &accounts.user3, &true);
        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&executed);
        client.execute(&executed);
        let consumed = soroban_sdk::vec![&env, executed];
        assert_eq!(
            client
                .try_propose(
                    &accounts.user3,
                    &target,
                    &payload(&env),
                    &DURATION,
                    &consumed,
                    &None
                )
                .unwrap_err()
                .unwrap(),
            ForgeError::InvalidInput
        );
    }

    #[test]
    fn propose_rejects_cycle_using_iterative_dependency_walk() {
        let (env, _token, _tc, contract_id, client, accounts) = bonded_env!();
        let target = env.register(MockTarget, ());
        let first = client.propose(
            &accounts.user1,
            &target,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        let second = client.propose(
            &accounts.user2,
            &target,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        // Simulate malformed stored edges forming candidate -> #1 -> #2 ->
        // candidate. Normal calls cannot create forward edges because
        // unknown ids are rejected; this exercises a length-three cycle.
        let mut first_record = client.get_proposal(&first);
        first_record.requires = soroban_sdk::vec![&env, second];
        let mut second_record = client.get_proposal(&second);
        second_record.requires = soroban_sdk::vec![&env, second + 1];
        env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .set(&DataKey::Proposal(first), &first_record);
            env.storage()
                .persistent()
                .set(&DataKey::Proposal(second), &second_record);
        });
        let edge = soroban_sdk::vec![&env, first];
        assert_eq!(
            client
                .try_propose(
                    &accounts.user2,
                    &target,
                    &payload(&env),
                    &DURATION,
                    &edge,
                    &None
                )
                .unwrap_err()
                .unwrap(),
            ForgeError::InvalidInput
        );
    }

    #[test]
    fn dependency_guard_orders_execution_and_keeps_failed_state_untouched() {
        let (env, client, accounts, dependency, target) = setup!();
        let requires = soroban_sdk::vec![&env, dependency];
        let consumer = client.propose(
            &accounts.user2,
            &target,
            &payload(&env),
            &DURATION,
            &requires,
            &None,
        );
        assert_eq!(client.get_dependencies(&consumer).requires, requires);
        client.vote(&dependency, &accounts.user2, &true);
        client.vote(&consumer, &accounts.user3, &true);
        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&consumer); // vote finalisation is allowed while waiting
        assert_eq!(
            client.get_proposal(&consumer).state,
            ProposalState::Succeeded
        );
        assert_eq!(
            client.try_execute(&consumer).unwrap_err().unwrap(),
            ForgeError::DeadlineReached
        );
        assert_eq!(
            client.get_proposal(&consumer).state,
            ProposalState::Succeeded
        );
        client.execute(&dependency);
        client.execute(&dependency);
        client.execute(&consumer);
        assert_eq!(
            client.get_proposal(&consumer).state,
            ProposalState::Executed
        );
    }

    #[test]
    fn cancelled_dependency_permanently_blocks_succeeded_consumer() {
        let (env, client, accounts, dependency, target) = setup!();
        let requires = soroban_sdk::vec![&env, dependency];
        let consumer = client.propose(
            &accounts.user2,
            &target,
            &payload(&env),
            &DURATION,
            &requires,
            &None,
        );
        client.vote(&consumer, &accounts.user3, &true);
        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&consumer);
        assert_eq!(
            client.get_proposal(&consumer).state,
            ProposalState::Succeeded
        );
        client.cancel_proposal(&dependency, &accounts.user1);
        assert_eq!(
            client.try_execute(&consumer).unwrap_err().unwrap(),
            ForgeError::ContractInvocationFailed
        );
        assert_eq!(
            client.get_proposal(&consumer).state,
            ProposalState::Succeeded
        );
    }

    #[test]
    fn executed_conflict_blocks_dispatch() {
        let (env, client, accounts, first, target) = setup!();
        let consumer = client.propose(
            &accounts.user2,
            &target,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &Some(first),
        );
        client.vote(&first, &accounts.user2, &true);
        client.vote(&consumer, &accounts.user3, &true);
        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&consumer);
        assert_eq!(
            client.get_proposal(&consumer).state,
            ProposalState::Succeeded
        );
        client.execute(&first);
        client.execute(&first);
        assert_eq!(
            client.try_execute(&consumer).unwrap_err().unwrap(),
            ForgeError::InvalidInput
        );
        assert_eq!(
            client.get_proposal(&consumer).state,
            ProposalState::Succeeded
        );
    }

    #[test]
    fn vote_records_support() {
        let (_env, client, accounts, proposal_id, _target_id) = setup!();
        client.vote(&proposal_id, &accounts.user2, &true);
        let proposal = client.get_proposal(&proposal_id);
        assert_eq!(proposal.for_votes, FUNDS);
        assert_eq!(proposal.against_votes, 0);
    }

    #[test]
    fn vote_records_against() {
        let (_env, client, accounts, proposal_id, _target_id) = setup!();
        client.vote(&proposal_id, &accounts.user2, &false);
        let proposal = client.get_proposal(&proposal_id);
        assert_eq!(proposal.for_votes, 0);
        assert_eq!(proposal.against_votes, FUNDS);
    }

    #[test]
    fn vote_twice_is_invalid() {
        let (_env, client, accounts, proposal_id, _target_id) = setup!();
        client.vote(&proposal_id, &accounts.user2, &true);
        let err = client
            .try_vote(&proposal_id, &accounts.user2, &true)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn initialize_is_one_time_and_vote_requires_initialization() {
        let (env, client, accounts, target_id) = unbonded!();
        let admin = Address::generate(&env);
        let sac = env.register_stellar_asset_contract_v2(admin);
        let token = sac.address();
        let token_admin = StellarAssetClient::new(&env, &token);
        client.configure_bond(&token, &BOND, &accounts.deployer);
        token_admin.mint(&accounts.user1, &FUNDS);
        let proposal_id = client.propose(
            &accounts.user1,
            &target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        let err = client
            .try_vote(&proposal_id, &accounts.user2, &true)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::NotInitialized);

        client.initialize(&token);
        assert_eq!(
            client
                .try_initialize(&Address::generate(&env))
                .unwrap_err()
                .unwrap(),
            ForgeError::AlreadyInitialized
        );
    }

    #[test]
    fn vote_weight_uses_unequal_token_balances() {
        let (env, token, _tc, _contract_id, client, accounts, target_id) = fresh_bond!();
        let token_admin = StellarAssetClient::new(&env, &token);
        // Existing fixture mints FUNDS to user2 and user3. Add unequal
        // balances to demonstrate that the weight is read from SEP-41.
        token_admin.mint(&accounts.user2, &17);
        token_admin.mint(&accounts.user3, &43);
        let id = client.propose(
            &accounts.user1,
            &target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        client.vote(&id, &accounts.user2, &true);
        client.vote(&id, &accounts.user3, &false);
        let proposal = client.get_proposal(&id);
        assert_eq!(proposal.for_votes, FUNDS + 17);
        assert_eq!(proposal.against_votes, FUNDS + 43);
    }

    #[test]
    fn vote_rejects_zero_balance() {
        let (_env, _token, _tc, _contract_id, client, accounts, target_id) = fresh_bond!();
        let id = client.propose(
            &accounts.user1,
            &target_id,
            &payload(&_env),
            &DURATION,
            &soroban_sdk::Vec::new(&_env),
            &None,
        );
        let voter = Address::generate(&_env);
        assert_eq!(
            client.try_vote(&id, &voter, &true).unwrap_err().unwrap(),
            ForgeError::InvalidInput
        );
        assert!(!client.has_voted(&id, &voter));
    }

    #[test]
    fn vote_weight_overflow_is_rejected() {
        let (env, _token, _tc, _contract_id, client, accounts, target_id) = fresh_bond!();
        let id = client.propose(
            &accounts.user1,
            &target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        let mut proposal = client.get_proposal(&id);
        proposal.for_votes = i128::MAX;
        env.as_contract(&_contract_id, || {
            env.storage()
                .persistent()
                .set(&DataKey::Proposal(id), &proposal);
        });
        assert_eq!(
            client
                .try_vote(&id, &accounts.user2, &true)
                .unwrap_err()
                .unwrap(),
            ForgeError::ArithmeticOverflow
        );
    }

    #[test]
    fn vote_after_deadline_is_rejected() {
        let (env, client, accounts, proposal_id, _target_id) = setup!();
        env.ledger().set_timestamp(START + DURATION);
        let err = client
            .try_vote(&proposal_id, &accounts.user2, &true)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::DeadlineReached);
    }

    #[test]
    fn vote_missing_proposal_is_not_found() {
        let (_env, client, accounts, _id, _target_id) = setup!();
        let err = client
            .try_vote(&999, &accounts.user2, &true)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn execute_before_deadline_is_invalid() {
        let (_env, client, _accounts, proposal_id, _target_id) = setup!();
        let err = client.try_execute(&proposal_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn execute_after_deadline_passes_majority() {
        let (env, client, accounts, proposal_id, _target_id) = setup!();
        client.vote(&proposal_id, &accounts.user2, &true);
        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&proposal_id);
        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Succeeded
        );
    }

    #[test]
    fn execute_after_deadline_defeats_minority() {
        let (env, client, accounts, proposal_id, _target_id) = setup!();
        client.vote(&proposal_id, &accounts.user2, &false);
        client.vote(&proposal_id, &accounts.user3, &true);
        client.vote(&proposal_id, &accounts.validator, &false);
        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&proposal_id);
        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Defeated
        );
    }

    #[test]
    fn execute_tie_is_defeated() {
        let (env, client, accounts, proposal_id, _target_id) = setup!();
        client.vote(&proposal_id, &accounts.user2, &true);
        client.vote(&proposal_id, &accounts.user3, &false);
        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&proposal_id);
        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Defeated
        );
    }

    #[test]
    fn execute_no_votes_is_defeated() {
        let (env, client, _accounts, proposal_id, _target_id) = setup!();
        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&proposal_id);
        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Defeated
        );
    }

    #[test]
    fn execute_twice_is_invalid() {
        let (env, client, accounts, proposal_id, _target_id) = setup!();
        client.vote(&proposal_id, &accounts.user2, &true);
        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&proposal_id); // Active -> Succeeded
        client.execute(&proposal_id); // Succeeded -> Executed
        let err = client.try_execute(&proposal_id).unwrap_err().unwrap(); // Executed -> rejected
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn execute_missing_proposal_is_not_found() {
        let (_env, client, _accounts, _id, _target_id) = setup!();
        let err = client.try_execute(&999).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn get_proposal_missing_is_not_found() {
        let (_env, client, _accounts, _id, _target_id) = setup!();
        let err = client.try_get_proposal(&999).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn cancel_active_proposal_succeeds() {
        let (_env, client, accounts, proposal_id, _target_id) = setup!();
        client.cancel_proposal(&proposal_id, &accounts.user1);
        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Cancelled
        );
    }

    #[test]
    fn cancel_immediately_after_propose_succeeds() {
        let (env, client, accounts, _id, target_id) = setup!();
        let fresh_id = client.propose(
            &accounts.user3,
            &target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        client.cancel_proposal(&fresh_id, &accounts.user3);
        assert_eq!(
            client.get_proposal(&fresh_id).state,
            ProposalState::Cancelled
        );
    }

    #[test]
    fn cancel_after_quorum_before_execute_succeeds() {
        let (env, client, accounts, proposal_id, _target_id) = setup!();
        client.vote(&proposal_id, &accounts.user2, &true);
        client.vote(&proposal_id, &accounts.user3, &true);
        env.ledger().set_timestamp(START + DURATION + 1);
        client.cancel_proposal(&proposal_id, &accounts.user1);
        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Cancelled
        );
        assert_eq!(client.get_proposal(&proposal_id).for_votes, FUNDS * 2);
    }

    #[test]
    fn cancel_by_non_proposer_is_unauthorized() {
        let (_env, client, accounts, proposal_id, _target_id) = setup!();
        let err = client
            .try_cancel_proposal(&proposal_id, &accounts.user2)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::Unauthorized);
    }

    #[test]
    fn cancel_missing_proposal_is_not_found() {
        let (_env, client, accounts, _id, _target_id) = setup!();
        let err = client
            .try_cancel_proposal(&999, &accounts.user1)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn double_cancel_is_invalid() {
        let (_env, client, accounts, proposal_id, _target_id) = setup!();
        client.cancel_proposal(&proposal_id, &accounts.user1);
        let err = client
            .try_cancel_proposal(&proposal_id, &accounts.user1)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn cancel_then_vote_is_invalid() {
        let (_env, client, accounts, proposal_id, _target_id) = setup!();
        client.cancel_proposal(&proposal_id, &accounts.user1);
        let err = client
            .try_vote(&proposal_id, &accounts.user2, &true)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn cancel_then_execute_is_invalid() {
        let (_env, client, accounts, proposal_id, _target_id) = setup!();
        client.cancel_proposal(&proposal_id, &accounts.user1);
        let err = client.try_execute(&proposal_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    #[test]
    fn cancel_after_execute_is_invalid() {
        let (env, client, accounts, proposal_id, _target_id) = setup!();
        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&proposal_id);
        let err = client
            .try_cancel_proposal(&proposal_id, &accounts.user1)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
    }

    // -------------------------------------------------------------------
    // Cross-Contract Invocation & State Verification (Issue 58)
    // -------------------------------------------------------------------

    #[test]
    fn execute_dispatches_to_mock_target_and_changes_state_once() {
        let (env, client, accounts, proposal_id, target_id) = setup!();
        let mock_target = MockTargetClient::new(&env, &target_id);
        assert_eq!(mock_target.count(), 0);
        assert_eq!(mock_target.last_payload(), None);

        // Pass proposal with strict majority:
        client.vote(&proposal_id, &accounts.user2, &true);
        env.ledger().set_timestamp(START + DURATION + 1);

        // First execute: finalises Active -> Succeeded (target invocation not yet run)
        client.execute(&proposal_id);
        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Succeeded
        );
        assert_eq!(mock_target.count(), 0);

        // Second execute: performs cross-contract call, transitions Succeeded -> Executed
        client.execute(&proposal_id);
        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Executed
        );
        assert_eq!(mock_target.count(), 1);
        assert_eq!(mock_target.last_payload(), Some(payload(&env)));

        // Third execute on Executed proposal: rejected, target state remains 1
        let err = client.try_execute(&proposal_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
        assert_eq!(mock_target.count(), 1);
    }

    #[test]
    fn execute_reverting_target_leaves_proposal_succeeded_and_target_unchanged() {
        let (env, _token, _tc, _contract_id, client, accounts) = bonded_env!();
        let reverting_target_id = env.register(RevertingTarget, ());

        let proposal_id = client.propose(
            &accounts.user1,
            &reverting_target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );

        client.vote(&proposal_id, &accounts.user2, &true);
        env.ledger().set_timestamp(START + DURATION + 1);

        // Finalise Active -> Succeeded
        client.execute(&proposal_id);
        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Succeeded
        );

        // Execution attempt against reverting target fails with ContractInvocationFailed
        let err = client.try_execute(&proposal_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::ContractInvocationFailed);

        // Failure ordering guarantee: proposal remains Succeeded (NOT Executed)
        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Succeeded
        );
    }

    #[test]
    fn defeated_and_still_active_proposals_cannot_be_executed() {
        let (env, client, accounts, proposal_id, target_id) = setup!();
        let mock_target = MockTargetClient::new(&env, &target_id);

        // 1. Still-Active proposal before deadline cannot be executed
        let err_active = client.try_execute(&proposal_id).unwrap_err().unwrap();
        assert_eq!(err_active, ForgeError::InvalidInput);
        assert_eq!(mock_target.count(), 0);

        // 2. Defeated proposal cannot be executed
        client.vote(&proposal_id, &accounts.user2, &false);
        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&proposal_id); // Transitions Active -> Defeated
        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Defeated
        );

        let err_defeated = client.try_execute(&proposal_id).unwrap_err().unwrap();
        assert_eq!(err_defeated, ForgeError::InvalidInput);
        assert_eq!(mock_target.count(), 0);
    }

    #[test]
    fn target_auth_requirement_is_not_satisfied_implicitly() {
        let (env, _token, _tc, _contract_id, client, accounts) = bonded_env!();
        let auth_target_id = env.register(AuthCheckingTarget, ());

        let proposal_id = client.propose(
            &accounts.user1,
            &auth_target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        client.vote(&proposal_id, &accounts.user2, &true);
        env.ledger().set_timestamp(START + DURATION + 1);

        client.execute(&proposal_id); // Succeeded
        let err = client.try_execute(&proposal_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::ContractInvocationFailed);
        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Succeeded
        );
    }

    #[test]
    fn execute_is_permissionless_callable_by_unrelated_account() {
        let (env, client, accounts, proposal_id, target_id) = setup!();
        let mock_target = MockTargetClient::new(&env, &target_id);

        client.vote(&proposal_id, &accounts.user2, &true);
        env.ledger().set_timestamp(START + DURATION + 1);

        // An account that did not propose or vote triggers finalisation and execution
        client.execute(&proposal_id); // user3 or any caller
        client.execute(&proposal_id);
        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Executed
        );
        assert_eq!(mock_target.count(), 1);
    }

    #[test]
    fn events_emitted_during_proposal_lifecycle() {
        use soroban_sdk::xdr::{self, ScVal};

        let (env, token, _tc, contract_id, client, accounts) = bonded_env!();
        let target_id = env.register(MockTarget, ());

        // 1. propose emits the SAC transfer, then Proposed then BondPosted —
        //    creation and the bond join are one transaction, both keyed by
        //    proposal_id, and the pull is observable as the token's event
        let proposal_id = client.propose(
            &accounts.user1,
            &target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        assert_eq!(env.events().all().events().len(), 3); // transfer + 2
        assert_transfer_event(&env, &accounts.user1, &contract_id, BOND);
        let events = dao_events(&env, &contract_id);
        assert_eq!(events.len(), 2);
        let event = &events[0];
        let xdr::ContractEventBody::V0(body) = &event.body;
        assert_eq!(
            body.topics.first().unwrap(),
            &ScVal::Symbol("proposed".try_into().unwrap())
        );
        assert_eq!(body.topics.get(1).unwrap(), &ScVal::U64(proposal_id));
        let event = &events[1];
        let xdr::ContractEventBody::V0(body) = &event.body;
        assert_eq!(
            body.topics.first().unwrap(),
            &ScVal::Symbol("bond_posted".try_into().unwrap())
        );
        assert_eq!(body.topics.get(1).unwrap(), &ScVal::U64(proposal_id));
        assert_eq!(
            env.events().all().filter_by_contract(&token).events().len(),
            1,
            "the third event belongs to the token contract"
        );

        // 2. vote emits VoteCast with topic proposal_id
        client.vote(&proposal_id, &accounts.user2, &true);
        let all = env.events().all();
        let events = all.events();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        let xdr::ContractEventBody::V0(body) = &event.body;
        assert_eq!(
            body.topics.first().unwrap(),
            &ScVal::Symbol("vote_cast".try_into().unwrap())
        );
        assert_eq!(body.topics.get(1).unwrap(), &ScVal::U64(proposal_id));
        let ScVal::Map(Some(map)) = &body.data else {
            panic!("VoteCast data must be a map");
        };
        let weight = map
            .iter()
            .find(|entry| entry.key == ScVal::Symbol("weight".try_into().unwrap()))
            .map(|entry| entry.val.clone())
            .unwrap();
        assert_eq!(
            weight,
            ScVal::I128(xdr::Int128Parts {
                hi: 0,
                lo: FUNDS as u64
            })
        );

        // 3. Negative assertion: failed duplicate vote emits no events
        let err = client
            .try_vote(&proposal_id, &accounts.user2, &true)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
        assert_eq!(env.events().all().events().len(), 0);

        // 4. Negative assertion: read-only get_proposal emits no events
        let _ = client.get_proposal(&proposal_id);
        assert_eq!(env.events().all().events().len(), 0);

        // 5. execute finalisation (Active -> Succeeded) emits Finalised
        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&proposal_id);
        let all = env.events().all();
        let events = all.events();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        let xdr::ContractEventBody::V0(body) = &event.body;
        assert_eq!(
            body.topics.first().unwrap(),
            &ScVal::Symbol("finalised".try_into().unwrap())
        );
        assert_eq!(body.topics.get(1).unwrap(), &ScVal::U64(proposal_id));

        // 6. execute invocation (Succeeded -> Executed) emits the refund
        //    transfer, Finalised and the bond release that pays the
        //    proposer back
        client.execute(&proposal_id);
        assert_eq!(env.events().all().events().len(), 3); // transfer + 2
        assert_transfer_event(&env, &contract_id, &accounts.user1, BOND);
        let events = dao_events(&env, &contract_id);
        assert_eq!(events.len(), 2);
        let event = &events[0];
        let xdr::ContractEventBody::V0(body) = &event.body;
        assert_eq!(
            body.topics.first().unwrap(),
            &ScVal::Symbol("finalised".try_into().unwrap())
        );
        assert_eq!(body.topics.get(1).unwrap(), &ScVal::U64(proposal_id));
        let event = &events[1];
        let xdr::ContractEventBody::V0(body) = &event.body;
        assert_eq!(
            body.topics.first().unwrap(),
            &ScVal::Symbol("bond_released".try_into().unwrap())
        );
        assert_eq!(body.topics.get(1).unwrap(), &ScVal::U64(proposal_id));
        assert_bond_released(
            &env,
            &contract_id,
            proposal_id,
            &token,
            BOND,
            &accounts.user1,
            false,
        );
    }

    #[test]
    fn events_emitted_on_defeated_proposal() {
        use soroban_sdk::xdr::{self, ScVal};

        let (env, token, _tc, contract_id, client, accounts) = bonded_env!();
        let target_id = env.register(MockTarget, ());

        let proposal_id = client.propose(
            &accounts.user1,
            &target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        client.vote(&proposal_id, &accounts.user2, &false);

        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&proposal_id);
        // Finalisation plus the forfeit: the bond leaves for the treasury
        // in the same invocation, so the SAC transfer event leads.
        assert_eq!(env.events().all().events().len(), 3); // transfer + 2
        assert_transfer_event(&env, &contract_id, &accounts.deployer, BOND);
        let events = dao_events(&env, &contract_id);
        assert_eq!(events.len(), 2);
        let event = &events[0];
        let xdr::ContractEventBody::V0(body) = &event.body;
        assert_eq!(
            body.topics.first().unwrap(),
            &ScVal::Symbol("finalised".try_into().unwrap())
        );
        assert_eq!(body.topics.get(1).unwrap(), &ScVal::U64(proposal_id));
        let event = &events[1];
        let xdr::ContractEventBody::V0(body) = &event.body;
        assert_eq!(
            body.topics.first().unwrap(),
            &ScVal::Symbol("bond_released".try_into().unwrap())
        );
        assert_eq!(body.topics.get(1).unwrap(), &ScVal::U64(proposal_id));
        assert_bond_released(
            &env,
            &contract_id,
            proposal_id,
            &token,
            BOND,
            &accounts.deployer,
            true,
        );
    }

    #[test]
    fn test_introspection_count_pagination_and_has_voted() {
        let (env, _token, _tc, _contract_id, client, accounts) = bonded_env!();
        let target_id = env.register(MockTarget, ());

        // 1. Initial uninitialized state
        assert_eq!(client.get_proposal_count(), 0);
        assert_eq!(client.get_proposals(&0, &10).len(), 0);

        // 2. Create 5 proposals
        for _ in 0..5 {
            client.propose(
                &accounts.user1,
                &target_id,
                &payload(&env),
                &DURATION,
                &soroban_sdk::Vec::new(&env),
                &None,
            );
        }
        assert_eq!(client.get_proposal_count(), 5);

        // 3. Test pagination bounds
        // Page 1: offset 0, limit 2 -> [p1, p2]
        let page1 = client.get_proposals(&0, &2);
        assert_eq!(page1.len(), 2);
        assert_eq!(page1.get(0).unwrap().proposal_id, 1);
        assert_eq!(page1.get(1).unwrap().proposal_id, 2);

        // Page 2: offset 2, limit 2 -> [p3, p4]
        let page2 = client.get_proposals(&2, &2);
        assert_eq!(page2.len(), 2);
        assert_eq!(page2.get(0).unwrap().proposal_id, 3);
        assert_eq!(page2.get(1).unwrap().proposal_id, 4);

        // Page 3: offset 4, limit 2 -> [p5]
        let page3 = client.get_proposals(&4, &2);
        assert_eq!(page3.len(), 1);
        assert_eq!(page3.get(0).unwrap().proposal_id, 5);

        // Past total count: offset 5, limit 2 -> []
        let past = client.get_proposals(&5, &2);
        assert_eq!(past.len(), 0);

        // Zero limit returns InvalidInput
        assert_eq!(
            client.try_get_proposals(&0, &0).unwrap_err().unwrap(),
            ForgeError::InvalidInput
        );

        // 4. Test has_voted
        // Before voting
        assert!(!client.has_voted(&1, &accounts.user2));

        // Vote on proposal 1
        client.vote(&1, &accounts.user2, &true);

        // After voting
        assert!(client.has_voted(&1, &accounts.user2));
        assert!(!client.has_voted(&1, &accounts.user1));

        // Query voting status on non-existent proposal ID
        assert_eq!(
            client
                .try_has_voted(&999, &accounts.user2)
                .unwrap_err()
                .unwrap(),
            ForgeError::NotFound
        );
    }

    // -------------------------------------------------------------------
    // Proposal bonds — configuration (Issue 142)
    // -------------------------------------------------------------------

    #[test]
    fn configure_bond_rejects_non_positive_amount() {
        let (env, client, accounts, _target_id) = unbonded!();
        let token = Address::generate(&env);

        let err = client
            .try_configure_bond(&token, &0, &accounts.deployer)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
        let err = client
            .try_configure_bond(&token, &-1, &accounts.deployer)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);

        // A rejected configuration writes nothing: still unconfigured, so
        // `propose` stays shut rather than free.
        assert_eq!(
            client.try_get_bond_config().unwrap_err().unwrap(),
            ForgeError::NotInitialized
        );
    }

    #[test]
    fn configure_bond_is_one_time() {
        let (env, client, accounts, _target_id) = unbonded!();
        let token = Address::generate(&env);
        client.configure_bond(&token, &BOND, &accounts.deployer);

        let err = client
            .try_configure_bond(&token, &(BOND * 2), &accounts.arbiter)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::AlreadyInitialized);

        // The first configuration wins and is what every read returns.
        let config = client.get_bond_config();
        assert_eq!(config.token, token);
        assert_eq!(config.amount, BOND);
        assert_eq!(config.treasury, accounts.deployer);
    }

    #[test]
    fn get_bond_config_requires_configuration() {
        let (_env, client, _accounts, _target_id) = unbonded!();
        assert_eq!(
            client.try_get_bond_config().unwrap_err().unwrap(),
            ForgeError::NotInitialized
        );
    }

    #[test]
    fn propose_without_bond_configuration_is_rejected() {
        let (env, client, accounts, target_id) = unbonded!();
        let err = client
            .try_propose(
                &accounts.user1,
                &target_id,
                &payload(&env),
                &DURATION,
                &soroban_sdk::Vec::new(&env),
                &None,
            )
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::NotInitialized);
        // No record, no counter: an unconfigured contract is unusable, not
        // spam-prone.
        assert_eq!(client.get_proposal_count(), 0);
        assert_eq!(
            client.try_get_proposal(&1).unwrap_err().unwrap(),
            ForgeError::NotFound
        );
    }

    // -------------------------------------------------------------------
    // Proposal bonds — posting the bond (transfer-before-state)
    // -------------------------------------------------------------------

    #[test]
    fn propose_pulls_the_bond_into_contract_custody() {
        let (env, _token, tc, contract_id, client, accounts, target_id) = fresh_bond!();
        assert_eq!(tc.balance(&accounts.user1), FUNDS);
        assert_eq!(tc.balance(&contract_id), 0);

        let proposal_id = client.propose(
            &accounts.user1,
            &target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );

        assert_eq!(tc.balance(&accounts.user1), FUNDS - BOND);
        assert_eq!(tc.balance(&contract_id), BOND);
        assert_eq!(bond_held(&env, &contract_id), BOND);

        let proposal = client.get_proposal(&proposal_id);
        assert_eq!(proposal.bond_state, BondState::Posted);
        assert_eq!(proposal.bond_amount, BOND);
        assert_eq!(proposal.bond_token, client.get_bond_config().token);
    }

    #[test]
    fn propose_fails_without_bond_balance_and_leaves_no_proposal() {
        let (env, token, tc, contract_id, client, _accounts, target_id) = fresh_bond!();
        // A proposer funded just below the bond: the pull must fail.
        let poor = Address::generate(&env);
        StellarAssetClient::new(&env, &token).mint(&poor, &(BOND - 1));

        let err = client
            .try_propose(
                &poor,
                &target_id,
                &payload(&env),
                &DURATION,
                &soroban_sdk::Vec::new(&env),
                &None,
            )
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::TokenTransferFailed);

        // Transfer-before-state: no record, no id, no custody, no event-side
        // state either — the whole invocation reverted.
        assert_eq!(client.get_proposal_count(), 0);
        assert_eq!(
            client.try_get_proposal(&1).unwrap_err().unwrap(),
            ForgeError::NotFound
        );
        assert_eq!(bond_held(&env, &contract_id), 0);
        assert_eq!(tc.balance(&contract_id), 0);
        assert_eq!(tc.balance(&poor), BOND - 1);
    }

    #[test]
    fn propose_rejects_when_custody_total_would_overflow_i128() {
        let (env, _token, tc, contract_id, client, accounts, target_id) = fresh_bond!();
        // Push the running custody total to the i128 boundary; the next
        // post would wrap, so it must be rejected instead.
        set_bond_held(&env, &contract_id, i128::MAX);

        let err = client
            .try_propose(
                &accounts.user1,
                &target_id,
                &payload(&env),
                &DURATION,
                &soroban_sdk::Vec::new(&env),
                &None,
            )
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::ArithmeticOverflow);

        assert_eq!(client.get_proposal_count(), 0);
        assert_eq!(
            client.try_get_proposal(&1).unwrap_err().unwrap(),
            ForgeError::NotFound
        );
        assert_eq!(bond_held(&env, &contract_id), i128::MAX);
        assert_eq!(tc.balance(&contract_id), 0);
        assert_eq!(tc.balance(&accounts.user1), FUNDS);
    }

    // -------------------------------------------------------------------
    // Proposal bonds — terminal outcomes (refund / forfeit)
    // -------------------------------------------------------------------

    #[test]
    fn executed_proposal_refunds_the_bond() {
        let (env, _token, tc, contract_id, client, accounts, target_id) = fresh_bond!();
        let proposal_id = client.propose(
            &accounts.user1,
            &target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        client.vote(&proposal_id, &accounts.user2, &true);

        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&proposal_id); // Active -> Succeeded
                                      // Not terminal yet: the bond stays in custody.
        assert_eq!(tc.balance(&contract_id), BOND);
        assert_eq!(tc.balance(&accounts.user1), FUNDS - BOND);

        client.execute(&proposal_id); // Succeeded -> Executed, bond refunded
        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Executed
        );
        assert_eq!(
            client.get_proposal(&proposal_id).bond_state,
            BondState::Refunded
        );
        assert_eq!(tc.balance(&accounts.user1), FUNDS);
        assert_eq!(tc.balance(&contract_id), 0);
        assert_eq!(bond_held(&env, &contract_id), 0);
    }

    #[test]
    fn cancelled_proposal_refunds_the_bond() {
        let (env, _token, tc, contract_id, client, accounts, target_id) = fresh_bond!();
        let proposal_id = client.propose(
            &accounts.user1,
            &target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        assert_eq!(tc.balance(&contract_id), BOND);

        client.cancel_proposal(&proposal_id, &accounts.user1);
        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Cancelled
        );
        assert_eq!(
            client.get_proposal(&proposal_id).bond_state,
            BondState::Refunded
        );
        assert_eq!(tc.balance(&accounts.user1), FUNDS);
        assert_eq!(tc.balance(&contract_id), 0);
        assert_eq!(bond_held(&env, &contract_id), 0);

        // The cancelled state gates a second payout.
        let err = client
            .try_cancel_proposal(&proposal_id, &accounts.user1)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
        assert_eq!(tc.balance(&accounts.user1), FUNDS);
        assert_eq!(tc.balance(&contract_id), 0);
    }

    #[test]
    fn defeated_proposal_forfeits_the_bond_to_the_treasury() {
        let (env, _token, tc, contract_id, client, accounts, target_id) = fresh_bond!();
        let proposal_id = client.propose(
            &accounts.user1,
            &target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        client.vote(&proposal_id, &accounts.user2, &false);

        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&proposal_id); // Active -> Defeated, bond forfeited

        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Defeated
        );
        assert_eq!(
            client.get_proposal(&proposal_id).bond_state,
            BondState::Forfeited
        );
        // The proposer keeps nothing back; the treasury takes the bond.
        assert_eq!(tc.balance(&accounts.user1), FUNDS - BOND);
        assert_eq!(tc.balance(&accounts.deployer), BOND);
        assert_eq!(tc.balance(&contract_id), 0);
        assert_eq!(bond_held(&env, &contract_id), 0);
    }

    #[test]
    fn bond_conservation_holds_across_every_terminal_path() {
        let (env, _token, tc, contract_id, client, accounts, target_id) = fresh_bond!();

        // One proposal per terminal path: executed (refund), cancelled
        // (refund), defeated (forfeit).
        let executed = client.propose(
            &accounts.user1,
            &target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        let cancelled = client.propose(
            &accounts.user2,
            &target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        let defeated = client.propose(
            &accounts.user3,
            &target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        client.vote(&executed, &accounts.user2, &true);
        client.vote(&defeated, &accounts.user2, &false);

        // Baseline after posting: three bonds in custody, nothing released.
        assert_eq!(tc.balance(&contract_id), BOND * 3);
        assert_eq!(bond_held(&env, &contract_id), BOND * 3);

        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&executed); // Active -> Succeeded
        client.execute(&executed); // Succeeded -> Executed (refund)
        client.cancel_proposal(&cancelled, &accounts.user2); // refund
        client.execute(&defeated); // Active -> Defeated (forfeit)

        // Exact per-path deltas from the posted baseline:
        assert_eq!(
            client.get_proposal(&executed).bond_state,
            BondState::Refunded
        );
        assert_eq!(
            client.get_proposal(&cancelled).bond_state,
            BondState::Refunded
        );
        assert_eq!(
            client.get_proposal(&defeated).bond_state,
            BondState::Forfeited
        );
        assert_eq!(tc.balance(&accounts.user1), FUNDS); // refund: back to start
        assert_eq!(tc.balance(&accounts.user2), FUNDS); // refund: back to start
        assert_eq!(tc.balance(&accounts.user3), FUNDS - BOND); // forfeited
        assert_eq!(tc.balance(&accounts.deployer), BOND); // treasury's cut
        assert_eq!(tc.balance(&accounts.validator), FUNDS); // untouched
        assert_eq!(tc.balance(&contract_id), 0);
        assert_eq!(bond_held(&env, &contract_id), 0);

        // Conservation: nothing minted, nothing burned — the four funded
        // accounts plus treasury plus contract still sum to the mint.
        let total = tc.balance(&accounts.user1)
            + tc.balance(&accounts.user2)
            + tc.balance(&accounts.user3)
            + tc.balance(&accounts.validator)
            + tc.balance(&accounts.deployer)
            + tc.balance(&contract_id);
        assert_eq!(total, FUNDS * 4);
    }

    #[test]
    fn bond_cannot_be_released_twice() {
        let (env, _token, tc, contract_id, client, accounts, target_id) = fresh_bond!();
        let proposal_id = client.propose(
            &accounts.user1,
            &target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        client.vote(&proposal_id, &accounts.user2, &true);
        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&proposal_id); // Active -> Succeeded
        client.execute(&proposal_id); // Succeeded -> Executed (refund paid)

        assert_eq!(tc.balance(&contract_id), 0);
        assert_eq!(tc.balance(&accounts.user1), FUNDS);

        // Every further release attempt is state-gated and moves nothing.
        let err = client.try_execute(&proposal_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
        let err = client
            .try_cancel_proposal(&proposal_id, &accounts.user1)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::InvalidInput);
        assert_eq!(tc.balance(&contract_id), 0);
        assert_eq!(tc.balance(&accounts.user1), FUNDS);
        assert_eq!(bond_held(&env, &contract_id), 0);
        assert_eq!(
            client.get_proposal(&proposal_id).bond_state,
            BondState::Refunded
        );
    }

    #[test]
    fn failed_bond_refund_reverts_the_whole_execution() {
        let (env, _token, tc, contract_id, client, accounts, target_id) = fresh_bond!();
        let mock_target = MockTargetClient::new(&env, &target_id);
        let proposal_id = client.propose(
            &accounts.user1,
            &target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        client.vote(&proposal_id, &accounts.user2, &true);
        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&proposal_id); // Active -> Succeeded, bond still held
        assert_eq!(mock_target.count(), 0);

        // Inflate the recorded bond past custody so the refund cannot pay.
        tamper_bond_amount(&env, &contract_id, proposal_id, BOND + 1);

        let err = client.try_execute(&proposal_id).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::TokenTransferFailed);

        // The failure surfaces with the proposal untouched — and the target
        // dispatch that preceded the refund is rolled back with it.
        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Succeeded
        );
        assert_eq!(
            client.get_proposal(&proposal_id).bond_state,
            BondState::Posted
        );
        assert_eq!(mock_target.count(), 0);
        assert_eq!(tc.balance(&contract_id), BOND);
        assert_eq!(tc.balance(&accounts.user1), FUNDS - BOND);
        assert_eq!(bond_held(&env, &contract_id), BOND);
    }

    #[test]
    fn bond_release_underflow_is_arithmetic_overflow_and_changes_nothing() {
        let (env, _token, tc, contract_id, client, accounts, target_id) = fresh_bond!();
        let proposal_id = client.propose(
            &accounts.user1,
            &target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        assert_eq!(bond_held(&env, &contract_id), BOND);

        // Drive the custody total to the i128 boundary: the release's
        // checked_sub would fall below i128::MIN, so the defeat path must
        // reject the arithmetic before any transfer or state write.
        set_bond_held(&env, &contract_id, i128::MIN);
        assert_eq!(bond_held(&env, &contract_id), i128::MIN);

        env.ledger().set_timestamp(START + DURATION + 1);
        let err = client.try_execute(&proposal_id).unwrap_err().unwrap(); // defeat path
        assert_eq!(err, ForgeError::ArithmeticOverflow);

        assert_eq!(
            client.get_proposal(&proposal_id).state,
            ProposalState::Active
        );
        assert_eq!(
            client.get_proposal(&proposal_id).bond_state,
            BondState::Posted
        );
        assert_eq!(bond_held(&env, &contract_id), i128::MIN);
        assert_eq!(tc.balance(&contract_id), BOND);
        assert_eq!(tc.balance(&accounts.deployer), 0); // nothing forfeited
        assert_eq!(tc.balance(&accounts.user1), FUNDS - BOND);
    }

    #[test]
    fn cancel_emits_a_bond_release_event() {
        use soroban_sdk::xdr::{self, ScVal};

        let (env, token, _tc, contract_id, client, accounts, target_id) = fresh_bond!();
        let proposal_id = client.propose(
            &accounts.user1,
            &target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );

        client.cancel_proposal(&proposal_id, &accounts.user1);
        // Cancellation emits no state event today; its bond release is the
        // observable, preceded by the refund transfer out of custody.
        assert_eq!(env.events().all().events().len(), 2); // transfer + 1
        assert_transfer_event(&env, &contract_id, &accounts.user1, BOND);
        let events = dao_events(&env, &contract_id);
        assert_eq!(events.len(), 1);
        let event = &events[0];
        let xdr::ContractEventBody::V0(body) = &event.body;
        assert_eq!(
            body.topics.first().unwrap(),
            &ScVal::Symbol("bond_released".try_into().unwrap())
        );
        assert_eq!(body.topics.get(1).unwrap(), &ScVal::U64(proposal_id));
        assert_bond_released(
            &env,
            &contract_id,
            proposal_id,
            &token,
            BOND,
            &accounts.user1,
            false,
        );
    }

    #[test]
    fn touch_ttl_extends_and_keeps_state_intact() {
        let (env, _token, _tc, _contract_id, client, accounts, target_id) = fresh_bond!();
        let proposal_id = client.propose(
            &accounts.user1,
            &target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        assert_eq!(client.touch_ttl(&proposal_id), ());
        let proposal = client.get_proposal(&proposal_id);
        assert_eq!(proposal.proposal_id, proposal_id);
        assert_eq!(proposal.state, ProposalState::Active);
    }

    #[test]
    fn touch_ttl_unknown_proposal_returns_not_found() {
        let (_env, _token, _tc, _contract_id, client, _accounts, _target_id) = fresh_bond!();
        let err = client.try_touch_ttl(&999).unwrap_err().unwrap();
        assert_eq!(err, ForgeError::NotFound);
    }

    #[test]
    fn proposer_cooldown_enforced_at_boundary() {
        let (env, _token, _tc, _contract_id, client, accounts, target_id) = fresh_bond!();
        assert_eq!(client.get_active_proposal_count(&accounts.user1), 0);

        for _ in 0..DEFAULT_MAX_ACTIVE_PROPOSALS {
            client.propose(
                &accounts.user1,
                &target_id,
                &payload(&env),
                &DURATION,
                &soroban_sdk::Vec::new(&env),
                &None,
            );
        }
        assert_eq!(
            client.get_active_proposal_count(&accounts.user1),
            DEFAULT_MAX_ACTIVE_PROPOSALS
        );

        // Exceeding the concurrent active proposals limit is rejected with ProposerCooldown.
        let err = client
            .try_propose(
                &accounts.user1,
                &target_id,
                &payload(&env),
                &DURATION,
                &soroban_sdk::Vec::new(&env),
                &None,
            )
            .unwrap_err()
            .unwrap();
        assert_eq!(err, ForgeError::ProposerCooldown);

        // Another proposer is unaffected by user1's limit.
        assert_eq!(client.get_active_proposal_count(&accounts.user2), 0);
        let id_u2 = client.propose(
            &accounts.user2,
            &target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        assert_eq!(client.get_active_proposal_count(&accounts.user2), 1);
        assert!(id_u2 > 0);
    }

    #[test]
    fn proposer_cooldown_decrements_on_cancel_and_execute() {
        let (env, _token, _tc, _contract_id, client, accounts, target_id) = fresh_bond!();
        let mut ids = std::vec::Vec::new();
        for _ in 0..DEFAULT_MAX_ACTIVE_PROPOSALS {
            ids.push(client.propose(
                &accounts.user1,
                &target_id,
                &payload(&env),
                &DURATION,
                &soroban_sdk::Vec::new(&env),
                &None,
            ));
        }
        assert_eq!(
            client.get_active_proposal_count(&accounts.user1),
            DEFAULT_MAX_ACTIVE_PROPOSALS
        );

        // Cancel one proposal: active count decreases by 1.
        client.cancel_proposal(&ids[0], &accounts.user1);
        assert_eq!(
            client.get_active_proposal_count(&accounts.user1),
            DEFAULT_MAX_ACTIVE_PROPOSALS - 1
        );

        // Now proposer can create a new proposal.
        let new_id = client.propose(
            &accounts.user1,
            &target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        assert_eq!(
            client.get_active_proposal_count(&accounts.user1),
            DEFAULT_MAX_ACTIVE_PROPOSALS
        );
        client.vote(&new_id, &accounts.user2, &true);

        // Defeat one proposal: vote against, advance past deadline, execute.
        client.vote(&ids[1], &accounts.user2, &false);
        env.ledger().set_timestamp(START + DURATION + 1);
        client.execute(&ids[1]); // becomes Defeated
        assert_eq!(
            client.get_active_proposal_count(&accounts.user1),
            DEFAULT_MAX_ACTIVE_PROPOSALS - 1
        );

        // Proposer can create another proposal.
        let _another_id = client.propose(
            &accounts.user1,
            &target_id,
            &payload(&env),
            &DURATION,
            &soroban_sdk::Vec::new(&env),
            &None,
        );
        assert_eq!(
            client.get_active_proposal_count(&accounts.user1),
            DEFAULT_MAX_ACTIVE_PROPOSALS
        );

        // Execute new_id: since vote was cast and deadline elapsed, finalize and dispatch.
        client.execute(&new_id); // Active -> Succeeded
        client.execute(&new_id); // Succeeded -> Executed
        assert_eq!(
            client.get_active_proposal_count(&accounts.user1),
            DEFAULT_MAX_ACTIVE_PROPOSALS - 1
        );
    }
}

// Negative authorization coverage for the bond-bearing entrypoints
// (`propose`, `cancel_proposal`, `execute`), following the escrow suite's
// two-layer pattern (issue #62's scope, extended to the new entrypoints).
#[cfg(test)]
mod authz;

// Randomized property suite covering the vote-tally, deadline/lifecycle,
// and finalization-outcome invariants (issue #62).
#[cfg(test)]
mod props;

#[cfg(test)]
mod indexer_fixtures;
