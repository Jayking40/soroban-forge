# DAO Governance Contract

On-chain proposal system with voting, bonded proposal creation, and
permissionless execution of approved opaque actions.

- **Source:** `crates/dao-governance`
- **Client:** `SorobanForgeDaoGovernanceClient` (generated)
- **Related:** [Contract index](./index.md), [Feature Status Matrix](../FEATURE-STATUS.md), [Known Limitations](../KNOWN-LIMITATIONS.md), [Multi-Sig Wallet](./multi-sig-wallet.md)

## Interface

```rust
fn initialize(governance_token) -> Result<(), ForgeError>
fn configure_bond(token, amount, treasury) -> Result<(), ForgeError>
fn get_bond_config() -> Result<BondConfig, ForgeError>
fn propose(proposer, target, action, duration, requires, conflicts_with) -> Result<u64, ForgeError>
fn get_dependencies(proposal_id) -> Result<DependencyView, ForgeError>
fn vote(proposal_id, voter, support) -> Result<(), ForgeError>
fn execute(proposal_id) -> Result<(), ForgeError>
fn cancel_proposal(proposal_id, proposer) -> Result<(), ForgeError>
fn get_proposal(proposal_id) -> Result<Proposal, ForgeError>
fn get_proposal_count() -> u64
fn get_proposals(offset, limit) -> Result<Vec<Proposal>, ForgeError>
fn has_voted(proposal_id, voter) -> Result<bool, ForgeError>
fn get_active_proposal_count(proposer) -> u32
fn touch_ttl(proposal_id) -> Result<(), ForgeError>
```

`initialize(governance_token)` must configure a SEP-41 token once before
voting; a second call returns `ForgeError::AlreadyInitialized`. `action` is
forwarded as a single `Bytes` argument to the target contract's `execute`
entrypoint. Each voter may vote once, and the current governance-token
balance at vote time is added to the selected tally. A zero-balance vote is
rejected with `ForgeError::InvalidInput`. Finalisation still requires a
strict weighted majority. After the deadline, the first `execute` call finalises the vote; a
succeeded proposal is then dispatched by a subsequent permissionless
`execute` call. Only a successful target invocation changes `Succeeded` to
`Executed`. A target revert returns
`ForgeError::ContractInvocationFailed` and leaves the proposal retryable in
`Succeeded`.

`requires` lists proposal ids that must reach `Executed` before the target
action can run. `conflicts_with` is one optional id; if it executes, this
proposal can never dispatch. Both edges are same-contract relations.
`get_dependencies` exposes them to indexers. Unknown ids return
`ForgeError::NotFound`; self edges, duplicate edges, consumed references,
and cycles return `ForgeError::InvalidInput`.

The DAO call itself is permissionless after voting has ended. A target's own
`require_auth` is not implicitly satisfied by the DAO's cross-contract call;
targets that require authorization must receive an authorization path that
the target contract accepts (see [Execute dispatch](#execute-dispatch)).

## State machine

Proposals progress through the following states, driven entirely by
`propose`, `vote`, `execute`, and `cancel_proposal`:

| State | Meaning | Entered from | Exits to |
|---|---|---|---|
| `Active` | Voting in progress | `propose` | `Succeeded`, `Defeated`, `Cancelled` |
| `Succeeded` | Strict `for` majority after the deadline; ready for dispatch | `execute` (finalization) | `Executed` |
| `Defeated` | No strict majority after the deadline (including ties and zero votes) | `execute` (finalization) | — (terminal) |
| `Executed` | Target dispatch succeeded | `execute` (dispatch) | — (terminal) |
| `Cancelled` | Withdrawn by the original proposer | `cancel_proposal` | — (terminal) |
| `Queued` | Reserved for an optional timelock; **not reachable** through the public interface | — | — |

```text
propose (voting_ends = now + duration)
  --> Active --vote × n--> deadline passes
  --> execute: for > against ? Succeeded : Defeated
  --> execute (on Succeeded): target.execute(action) --> Executed (terminal)
  --> cancel (proposer only): Cancelled (terminal)
```

Transition rules enforced by the contract:

- `vote` requires the proposal to be `Active` and the deadline not yet
  reached (`ForgeError::DeadlineReached` otherwise). It requires the voter to
  authorize, reads their SEP-41 balance, and adds that weight; one vote per
  voter regardless of balance. Voting before `initialize` returns
  `ForgeError::NotInitialized`.
- `execute` before the deadline is `ForgeError::InvalidInput`.
- On `Active` past deadline, `execute` finalizes: `for_votes > against_votes`
  → `Succeeded` (bond stays in custody); otherwise → `Defeated` (bond
  forfeited to the treasury). Ties and zero-vote proposals are `Defeated`.
- A `Succeeded` proposal is dispatched by a **second, separate** `execute`
  call; dispatch is described in [Execute dispatch](#execute-dispatch).
- `Defeated`, `Executed`, `Cancelled`, and still-`Active` proposals reject
  `execute` with `ForgeError::InvalidInput`.
- `cancel_proposal` requires the original proposer
  (`ForgeError::Unauthorized` otherwise) and an `Active` proposal; it works
  even after the deadline and after quorum is met, as long as the proposal
  has not been executed or cancelled.

## Proposer cooldown and active proposal limit

To bound proposal creation and prevent spam, each proposer can have at most
`DEFAULT_MAX_ACTIVE_PROPOSALS = 5` concurrent active proposals. Exceeding the
limit returns `ForgeError::ProposerCooldown`. The active count increments on
successful `propose` and decrements on `Cancelled`, `Defeated`, or `Executed`.
`get_active_proposal_count(proposer)` is a read-only view.

### Dependency outcomes

| Dependency condition | Consumer state | `execute` result | Resulting consumer state |
|---|---|---|---|
| Required proposal is Active or Succeeded | Succeeded | `DeadlineReached` | Succeeded; bond remains held |
| Required proposal is Cancelled or Defeated | Succeeded | `ContractInvocationFailed` | Succeeded permanently blocked |
| Required proposal is Executed | Succeeded | target dispatch proceeds | Executed on success |
| Conflicting proposal is Executed | Succeeded | `InvalidInput` | Succeeded permanently blocked |
| Conflicting proposal is not Executed | Succeeded | target dispatch proceeds | Executed on success |

The first `execute` on a passed proposal may move it to `Succeeded` while a
dependency is unresolved. The dependency guard runs before target dispatch,
refund, and state writes. A blocked call leaves the Succeeded proposal and its
bond unchanged. If a required proposal is later cancelled or defeated, its
dependents remain Succeeded and permanently unexecutable; they are not
automatically cancelled and their bonds stay held. This defines the
cancellation boundary for the separate amendment/cancellation work in #226.

Cycles are checked at proposal time with iterative DFS over reachable
`requires` edges. The traversal uses an explicit Soroban `Vec` as its stack
and a visited set, so it does not consume recursive call-stack depth. Runtime
is O(V + E) for the reachable dependency subgraph. Since ids must already
exist, ordinary edges point to older proposals; the traversal also catches
cycles in malformed stored graphs. `Queued` remains an orthogonal future time
gate: a timelock can queue work after its dependencies resolve, but is not
implemented here.

## Proposal bonds

Every proposal is backed by a bond in a SEP-41 token: paid when the
proposal is created, settled when it reaches a terminal state.

The governance token configured by `initialize` may be the same token as the
proposal bond token or a different token; bond amounts never contribute to
vote weight. Its configuration is stored in instance storage under the
additive `DataKey::GovernanceToken` key. Existing deployments must call
`initialize` once before accepting votes. Proposal records add the dependency
fields. The `Proposed` event name and topics stay unchanged; its `data` now
includes those fields.

This changes the `propose` client signature and serialized `Proposal` shape.
Consumers must regenerate the DAO client bindings. Existing deployed proposal
records require an explicit migration before upgrading this contract; this
feature does not implement that migration, so deploy a new DAO instance when
legacy proposals must remain readable.

**Configuration.** `configure_bond(token, amount, treasury)` is a one-time,
permissionless write — first caller wins, later calls return
`ForgeError::AlreadyInitialized`, mirroring the multi-sig `initialize`
pattern. The treasury address is fixed here, once, rather than chosen by
the party that later receives forfeited bonds. A non-positive amount is
rejected with `ForgeError::InvalidInput`. While no bond is configured,
`propose` returns `ForgeError::NotInitialized`: free proposals are never
accepted (an unconfigured deployment is unusable, not spam-prone).

**Posting.** `propose` pulls `amount` of `token` from the proposer into
contract custody *before* any state write, then stores `bond_token`,
`bond_amount`, and `bond_state = Posted` on the proposal and increments the
running custody total (`BondHeld`). A proposer without sufficient balance
fails with `ForgeError::TokenTransferFailed` and no proposal is created.

**Settlement.** Release happens in the same frame as the single terminal
transition — there is no separate refund call, and a bond can only be
released once:

| Transition | Trigger | Bond |
|---|---|---|
| `Active → Succeeded` | majority after the deadline | stays in custody (`Posted`) |
| `Succeeded → Executed` | successful target dispatch | refunded to the proposer (`Refunded`) |
| `Active → Defeated` | no majority after the deadline | forfeited to the configured treasury (`Forfeited`) |
| `Active → Cancelled` | proposer revokes the proposal | refunded to the proposer (`Refunded`) |

`BondState` mirrors this exactly: `Posted` (in custody), `Refunded` (back
with the proposer), or `Forfeited` (paid to the treasury).

**Ordering and failure.** Each path validates first (proposal state,
deadline, checked custody arithmetic), then moves the token, then writes
state and emits events — the same transfer-before-state discipline as the
[escrow contract](./escrow.md). A failed transfer reverts the entire
invocation — including a target dispatch that already ran — so the proposal
stays retryable and the custody total never drifts. Token failures are
bucketed as `ForgeError::TokenTransferFailed`; custody arithmetic that would
cross the `i128` boundary surfaces as `ForgeError::ArithmeticOverflow`. The
outgoing transfers need no external signer: contract self-authorization is
implicit in Soroban.

## Execute dispatch

Delivering an approved action is a two-step process, both steps
permissionless and keyed by `execute(proposal_id)`:

1. **Finalization** (proposal `Active` past the deadline): the tally is
   frozen. A strict `for` majority moves the proposal to `Succeeded` — the
   bond remains in custody and **no target call is made yet**. Otherwise the
   proposal becomes `Defeated` and the bond is forfeited.
2. **Dispatch** (proposal `Succeeded`): the contract performs a real
   cross-contract call to `proposal.target`'s `execute` entrypoint with the
   stored `action` payload as its sole argument:

   ```rust
   let args = soroban_sdk::vec![&env, proposal.action.into_val(&env)];
   let result = env.try_invoke_contract::<(), ForgeError>(
       &proposal.target,
       &Symbol::new(&env, "execute"),
       args,
   );
   ```

   - **On success:** the bond is refunded to the proposer, the proposal
     transitions to `Executed`, and `Finalised` + `BondReleased` events are
     emitted. The proposal is now terminal.
   - **On target revert:** `ForgeError::ContractInvocationFailed` is
     returned, the proposal **stays `Succeeded`**, the bond stays in
     custody, and the target's state is unchanged — the dispatch can be
     re-attempted by anyone.
   - **On refund failure:** the whole invocation reverts, including the
     already-executed target dispatch; the proposal stays `Succeeded` and
     retryable.

Because the tuple return is `Result<Result<(), ForgeError>, HostError>`, a
host-level abort (e.g. an undeployed target or a missing entrypoint) is
indistinguishable from a typed revert and both are surfaced as
`ForgeError::ContractInvocationFailed`.

**Target contract shape.** Any contract that exposes a public
`fn execute(env: Env, action: Bytes)` entrypoint can be a DAO target — see
`MockTarget`/`RevertingTarget` in the crate's test suite and the
`AuthCheckingTarget` test, which demonstrates that a target's own
`require_auth` is *not* satisfied by the DAO's call and causes
`ContractInvocationFailed`.

## End-to-end walkthrough

A full lifecycle, from deployment to on-chain effect:

1. **Deploy + configure.** Register `DaoGovernance`, then call
   `initialize(&governance_token)` and
   `configure_bond(&bond_token, &100, &treasury)` once in the deploy
   transaction. Both configurations are immutable; there is no admin role.
   `propose` is rejected until the bond is configured, and `vote` is rejected
   until the governance token is configured.

2. **Create a proposal.** A member calls
   `propose(&proposer, &target, &action_payload, &duration)`. The contract
   transfers 100 units of the bond token from the proposer into custody,
   assigns the next stable `proposal_id`, records the proposal as `Active`
   with `voting_ends = now + duration`, and emits `Proposed` and
   `BondPosted`. The proposer's signature covers the nested bond pull.

3. **Vote.** Each member calls `vote(&proposal_id, &voter, &support)` once.
   The contract reads the voter's current governance-token balance and adds
   it to `for_votes` or `against_votes`, emits `VoteCast` with that weight,
   and rejects zero-balance voters, duplicate votes, votes after the deadline,
   and votes on non-`Active` proposals.

4. **Finalize.** After `voting_ends`, anyone (not just voters — the
   proposal creator or an observer) calls `execute(&proposal_id)`. A strict
   `for` majority moves the proposal to `Succeeded` (`Finalised` event);
   otherwise it becomes `Defeated` and the bond is forfeited to the
   treasury (`Finalised` + `BondReleased` with `forfeited: true`).

5. **Dispatch.** Anyone calls `execute(&proposal_id)` again. The contract
   invokes `target.execute(action)`. On success it refunds the bond to the
   proposer, marks the proposal `Executed`, and emits `Finalised` +
   `BondReleased` with `forfeited: false`. On failure it stays `Succeeded`
   and can be retried.

6. **Cancel (alternative).** While the proposal is still `Active`, the
   original proposer may call `cancel_proposal(&proposal_id, &proposer)` to
   refund the bond and freeze the proposal as `Cancelled` — even after the
   deadline, as long as it has not been executed.

7. **Keep alive (optional).** A keeper periodically calls
   `touch_ttl(&proposal_id)` to extend the 30-day TTL horizon of long-lived
   proposals (see [Storage & TTL](#storage--ttl-maintenance)).

Indexers reconstruct the full lifecycle from the event stream alone:
`Proposed` → `VoteCast` × n → `Finalised` → (optional `BondReleased`), each
keyed by `proposal_id`.

## Events

The contract emits typed on-chain lifecycle events for indexers and off-chain
monitoring, each keyed by the standard `proposal_id` topic:

- **`Proposed`** — emitted by `propose` once the proposal record and bond
  are in place.
  - Topics: `proposal_id: u64`
  - Data: `data: Proposal` (the full initial record)
- **`VoteCast`** — emitted by `vote` for each accepted vote.
  - Topics: `proposal_id: u64`
  - Data: `voter: Address`, `support: bool`, `weight: i128`
- **`Finalised`** — emitted by `execute` on every state transition
  (`Active` → `Succeeded`, `Active` → `Defeated`, `Succeeded` → `Executed`).
  - Topics: `proposal_id: u64`
  - Data: `state: ProposalState`, `for_votes: i128`, `against_votes: i128`
- **`BondPosted`** — emitted by `propose` once the bond is in custody.
  - Topics: `proposal_id: u64`
  - Data: `token: Address`, `amount: i128`
- **`BondReleased`** — emitted by `execute` (refund or forfeit) and
  `cancel_proposal` (refund) when a bond leaves custody.
  - Topics: `proposal_id: u64`
  - Data: `token: Address`, `amount: i128`, `to: Address`,
    `forfeited: bool` (`true` = sent to the treasury)

Whenever a bond moves, the bond token's own `transfer` event appears in the
same invocation. Indexers should filter events by the emitting contract
address to separate the DAO's records from the token's.

## Storage & TTL Maintenance

Proposal records (`DataKey::Proposal(u64)`) are stored in persistent storage. `DataKey::Count`, `DataKey::Bond`, `DataKey::BondHeld`, and `DataKey::Vote` entries remain in instance storage.

`propose`, `vote`, `execute`, and `cancel_proposal` extend proposal persistent storage TTL on every write to a 30-day horizon (`30 * DAY_IN_LEDGERS = 518,400` ledgers). Dependency edges live on the proposal record; no top-level storage key is added.

A permissionless public keeper entrypoint `touch_ttl(proposal_id)` allows anyone to bump a proposal's persistent TTL without modifying its state. If the proposal ID does not exist, `touch_ttl` returns `ForgeError::NotFound`.
