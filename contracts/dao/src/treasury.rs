use soroban_sdk::{symbol_short, Address, Env, String};

use crate::error::Error;
use crate::storage;
use crate::types::{ProposalStatus, TreasuryProposal};
use crate::util;

// `env.events().publish` is deprecated in soroban-sdk in favour of
// `#[contractevent]`, but migration is a coordinated, breaking wire-format
// change (#85).  Suppress per-function so unrelated deprecations still surface.
#[allow(deprecated)]
pub fn propose_withdrawal(
    env: &Env,
    proposer: Address,
    amount: i128,
    destination: Address,
    reason: String,
    private: bool,
) -> Result<u32, Error> {
    util::require_initialized(env)?;
    util::require_not_paused(env)?;
    util::require_active_member(env, &proposer)?;

    if amount <= 0 {
        return Err(Error::InvalidAmount);
    }
    if amount > util::treasury_balance(env) {
        return Err(Error::InsufficientTreasury);
    }

    let id = storage::next_id(env, storage::DataKey::NextTreasuryId);
    let proposal = TreasuryProposal {
        id,
        proposer,
        amount,
        destination: destination.clone(),
        reason,
        created_at: env.ledger().timestamp(),
        status: ProposalStatus::Pending,
        for_votes: 0,
        against_votes: 0,
        votes_cast: 0,
        voting_period: storage::get_policy(env).voting_period,
        treasury_threshold: storage::get_policy(env).treasury_threshold,
        private,
        active_members: storage::get_active_members(env),
    };
    storage::set_treasury_proposal(env, &proposal);
    storage::extend_instance(env);

    env.events().publish(
        (symbol_short!("tre_prop"),),
        (id, amount, destination, private),
    );
    Ok(id)
}

pub fn vote(env: &Env, voter: Address, proposal_id: u32, support: bool) -> Result<(), Error> {
    util::require_initialized(env)?;
    util::require_not_paused(env)?;
    util::require_active_member(env, &voter)?;

    let proposal =
        storage::get_treasury_proposal(env, proposal_id).ok_or(Error::TreasuryProposalNotFound)?;
    if proposal.private {
        // Private proposals must go through commit → reveal, not open voting.
        return Err(Error::NotAuthorized);
    }
    tally(env, proposal, &voter, support)
}

/// Shared vote-recording + execution path. Used by open voting and by the
/// commit-reveal privacy module once a vote is revealed. Assumes the caller has
/// already authorized `voter` and enforced any privacy-mode rules.
#[allow(deprecated)]
pub fn tally(
    env: &Env,
    mut proposal: TreasuryProposal,
    voter: &Address,
    support: bool,
) -> Result<(), Error> {
    if proposal.status != ProposalStatus::Pending {
        return Err(Error::NotInVotingPhase);
    }
    // Treasury proposals never expired before this check (#17) — a Pending
    // proposal could sit indefinitely and still be voted through/executed
    // long after the intended voting window. Mirror the loan proposal
    // lifecycle: once VOTING_PERIOD has elapsed since creation, persist the
    // Expired status and reject further votes.
    if env.ledger().timestamp() > proposal.created_at + proposal.voting_period {
        return Err(Error::NotInVotingPhase);
    }
    if storage::has_treasury_voted(env, proposal.id, voter) {
        return Err(Error::AlreadyVoted);
    }
    // Callers have already checked `voter` is an active member; this adds the
    // electorate rule for both open votes and commit-reveal reveals (#73).
    let member = storage::get_member(env, voter).ok_or(Error::NotMember)?;
    util::require_in_electorate(&member, proposal.created_at)?;

    let weight = util::voting_weight(env, voter);
    if support {
        proposal.for_votes += weight;
    } else {
        proposal.against_votes += weight;
    }
    proposal.votes_cast += 1;
    storage::set_treasury_voted(env, proposal.id, voter);
    env.events().publish(
        (symbol_short!("tre_vote"),),
        (proposal.id, voter.clone(), support),
    );

    // Quorum is fixed against the electorate snapshotted at creation (#73).
    let required = util::required_votes(proposal.active_members, proposal.treasury_threshold);
    if proposal.for_votes >= required {
        proposal.status = ProposalStatus::ApprovedPendingDisbursement;
        storage::set_treasury_proposal(env, &proposal);
        if execute(env, &mut proposal).is_err() {
            env.events()
                .publish((symbol_short!("tre_wait"),), (proposal.id, proposal.amount));
        }
    } else {
        let remaining = proposal.active_members.saturating_sub(proposal.votes_cast);
        let max_remaining = remaining as i128 * (1 + util::MAX_STAKE_BONUS);
        if proposal.for_votes + max_remaining < required {
            proposal.status = ProposalStatus::Rejected;
            env.events().publish(
                (symbol_short!("tre_rej"),),
                (proposal.id, proposal.for_votes, proposal.against_votes),
            );
        }
    }
    storage::set_treasury_proposal(env, &proposal);
    Ok(())
}

/// Executes a treasury withdrawal whose proposal already passed its vote but
/// could not be paid out at the time because the treasury was short (status
/// `ApprovedPendingDisbursement`, see `tre_wait`). Exposed as
/// `execute_treasury_proposal`. Permissionless and callable by anyone, member
/// or not — a keeper call, like `loans::mark_loan_defaulted`, not a
/// privileged action, so there's nothing to authorize.
///
/// Why no auth is needed: the members already approved this exact transfer.
/// The `amount` and `destination` are fixed on the stored proposal, which the
/// vote approved as-is. The caller passes only `proposal_id` and cannot
/// change who is paid or how much.
///
/// What the caller can influence: only *when* the transfer happens, and only
/// once the treasury can cover it. The caller pays the transaction fee and
/// receives nothing, so there is no incentive to grief.
///
/// Failure mode: if the treasury still can't cover the amount this returns
/// `InsufficientTreasury` and nothing changes. Repeated calls are safe — once
/// executed, the status is `Executed` and the status guard below rejects every
/// further call, so a withdrawal can never be paid twice.
///
/// Do not add a `require_auth` here: requiring an admin or the proposer would
/// let a single absent party block an outcome the DAO already voted for. The
/// `non_member_can_execute_approved_treasury_proposal_without_auth` test pins
/// this.
pub fn execute_approved(env: &Env, proposal_id: u32) -> Result<(), Error> {
    util::require_initialized(env)?;
    util::require_not_paused(env)?;
    let mut proposal =
        storage::get_treasury_proposal(env, proposal_id).ok_or(Error::TreasuryProposalNotFound)?;
    if proposal.status != ProposalStatus::ApprovedPendingDisbursement {
        return Err(Error::NotInVotingPhase);
    }
    execute(env, &mut proposal)?;
    storage::set_treasury_proposal(env, &proposal);
    Ok(())
}

#[allow(deprecated)]
fn execute(env: &Env, proposal: &mut TreasuryProposal) -> Result<(), Error> {
    if util::treasury_balance(env) < proposal.amount {
        return Err(Error::InsufficientTreasury);
    }
    proposal.status = ProposalStatus::Executed;
    util::token_client(env).transfer(
        &util::contract_address(env),
        &proposal.destination,
        &proposal.amount,
    );
    env.events().publish(
        (symbol_short!("tre_exec"),),
        (proposal.id, proposal.amount, proposal.destination.clone()),
    );
    Ok(())
}


pub fn expire_treasury_proposal(env: &Env, proposal_id: u32) -> Result<(), Error> {
    util::require_initialized(env)?;
    util::require_not_paused(env)?;
    let mut proposal = storage::get_treasury_proposal(env, proposal_id)
        .ok_or(Error::TreasuryProposalNotFound)?;
    
    if proposal.status != ProposalStatus::Pending {
        return Err(Error::NotInVotingPhase); // Re-using error
    }
    
    if env.ledger().timestamp() > proposal.created_at + proposal.voting_period {
        proposal.status = ProposalStatus::Expired;
        storage::set_treasury_proposal(env, &proposal);
        // Emitting an event would be nice but not strictly required.
        Ok(())
    } else {
        Err(Error::NotInVotingPhase) // Re-using error
    }
}