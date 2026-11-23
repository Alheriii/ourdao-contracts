use soroban_sdk::testutils::Address as _;
use soroban_sdk::{Address, String};

use super::common::*;
use crate::types::ProposalStatus;
use crate::Error;

#[test]
fn treasury_withdrawal_open_vote() {
    let s = setup(3);
    let proposer = s.members.get(0).unwrap();
    let v1 = s.members.get(1).unwrap();
    let v2 = s.members.get(2).unwrap();
    let dest = Address::generate(&s.env);

    let reason = String::from_str(&s.env, "grant");
    let pid = s
        .client
        .propose_treasury_withdrawal(&proposer, &600, &dest, &reason, &false);

    s.client.vote_on_treasury_proposal(&v1, &pid, &true);
    let mid = s.client.get_treasury_proposal(&pid).unwrap();
    assert_eq!(mid.status, ProposalStatus::Pending); // 1 vote not enough (needs 2)

    s.client.vote_on_treasury_proposal(&v2, &pid, &true);
    let done = s.client.get_treasury_proposal(&pid).unwrap();
    assert_eq!(done.status, ProposalStatus::Executed);
    assert_eq!(s.token.balance(&dest), 600);
}

#[test]
fn approved_but_unfundable_treasury_withdrawal_waits_then_executes_after_refill() {
    // 4 members => treasury 4000, 3 of 4 votes required.
    let s = setup(4);
    let p = s.members.get(0).unwrap();
    let v1 = s.members.get(1).unwrap();
    let v2 = s.members.get(2).unwrap();
    let dest = Address::generate(&s.env);
    let reason = String::from_str(&s.env, "grant");

    // Two withdrawals that each fit today but not both together.
    let first = s
        .client
        .propose_treasury_withdrawal(&p, &2_000, &dest, &reason, &false);
    let second = s
        .client
        .propose_treasury_withdrawal(&p, &2_500, &dest, &reason, &false);

    for voter in [&p, &v1, &v2] {
        s.client.vote_on_treasury_proposal(voter, &first, &true);
    }
    assert_eq!(s.client.get_treasury_balance(), 2_000);
    assert_eq!(s.token.balance(&dest), 2_000);

    s.client.vote_on_treasury_proposal(&p, &second, &true);
    s.client.vote_on_treasury_proposal(&v1, &second, &true);
    s.client.vote_on_treasury_proposal(&v2, &second, &true); // approved, but only 2000 left

    let prop = s.client.get_treasury_proposal(&second).unwrap();
    assert_eq!(prop.status, ProposalStatus::ApprovedPendingDisbursement);
    assert!(emitted(&s.env, "tre_wait"));
    assert_eq!(s.token.balance(&dest), 2_000); // nothing more moved
    assert_eq!(s.client.get_treasury_balance(), 2_000);
    assert_eq!(
        s.client.try_execute_treasury_proposal(&second),
        Err(Ok(Error::InsufficientTreasury))
    );

    refill_treasury(&s);
    refill_treasury(&s);
    s.client.execute_treasury_proposal(&second);

    let prop = s.client.get_treasury_proposal(&second).unwrap();
    assert_eq!(prop.status, ProposalStatus::Executed);
    assert_eq!(s.token.balance(&dest), 4_500);
}

#[test]
fn rejected_treasury_transfer_rolls_back_approval_vote_and_execution_state() {
    let s = rejecting_setup(3);
    let proposer = s.members.get(0).unwrap();
    let voter_one = s.members.get(1).unwrap();
    let voter_two = s.members.get(2).unwrap();
    let destination = Address::generate(&s.env);

    let proposal_id = s.client.propose_treasury_withdrawal(
        &proposer,
        &500,
        &destination,
        &String::from_str(&s.env, "rollback test"),
        &false,
    );

    s.client
        .vote_on_treasury_proposal(&voter_one, &proposal_id, &true);
    let before = s.client.get_treasury_proposal(&proposal_id).unwrap();
    assert_eq!(before.status, ProposalStatus::Pending);
    assert_eq!(before.for_votes, 1);
    assert_eq!(before.votes_cast, 1);

    s.token.set_reject_transfers(&true);
    let result = s
        .client
        .try_vote_on_treasury_proposal(&voter_two, &proposal_id, &true);
    assert!(result.is_err());

    let after = s.client.get_treasury_proposal(&proposal_id).unwrap();
    assert_eq!(after, before, "proposal changes must roll back");
    let has_vote = s.env.as_contract(&s.client.address, || {
        crate::storage::has_treasury_voted(&s.env, proposal_id, &voter_two)
    });
    assert!(
        !has_vote,
        "vote marker must not survive a rejected execution transfer"
    );
    assert_eq!(s.token.balance(&destination), 0);
}

// #71: `execute_treasury_proposal` is a deliberate permissionless keeper call.
// This pins that intent: a non-member with no auth at all can drive it, so
// adding a `require_auth` to the entrypoint makes this test fail.
#[test]
fn non_member_can_execute_approved_treasury_proposal_without_auth() {
    // 4 members => treasury 4000, 3 of 4 votes required.
    let s = setup(4);
    let p = s.members.get(0).unwrap();
    let v1 = s.members.get(1).unwrap();
    let v2 = s.members.get(2).unwrap();
    let dest = Address::generate(&s.env);
    let reason = String::from_str(&s.env, "grant");

    let first = s
        .client
        .propose_treasury_withdrawal(&p, &2_000, &dest, &reason, &false);
    let second = s
        .client
        .propose_treasury_withdrawal(&p, &2_500, &dest, &reason, &false);
    for voter in [&p, &v1, &v2] {
        s.client.vote_on_treasury_proposal(voter, &first, &true);
    }
    for voter in [&p, &v1, &v2] {
        s.client.vote_on_treasury_proposal(voter, &second, &true);
    }
    assert_eq!(
        s.client.get_treasury_proposal(&second).unwrap().status,
        ProposalStatus::ApprovedPendingDisbursement
    );
    refill_treasury(&s);
    refill_treasury(&s);

    let keeper = Address::generate(&s.env);
    assert!(s.client.get_member(&keeper).is_none());
    let keeper_bal = s.token.balance(&keeper);
    let dest_bal = s.token.balance(&dest);

    // No mocked auths from here on: any `require_auth` in the path would fail.
    s.env.set_auths(&[]);
    s.client.execute_treasury_proposal(&second);
    assert!(s.env.auths().is_empty());

    assert_eq!(
        s.client.get_treasury_proposal(&second).unwrap().status,
        ProposalStatus::Executed
    );
    // Funds go to the destination fixed on the proposal; the keeper gets nothing.
    assert_eq!(s.token.balance(&dest), dest_bal + 2_500);
    assert_eq!(s.token.balance(&keeper), keeper_bal);

    // Repeated calls are rejected by the status guard: no double payout.
    assert_eq!(
        s.client.try_execute_treasury_proposal(&second),
        Err(Ok(Error::NotInVotingPhase))
    );
    assert_eq!(s.token.balance(&dest), dest_bal + 2_500);
}

// ── #73: quorum is fixed at proposal creation ────────────────────────────────

#[test]
fn members_exiting_mid_vote_do_not_lower_treasury_quorum() {
    // 10 members at creation, 51% => 6 votes required.
    let s = setup(10);
    let dest = Address::generate(&s.env);
    let reason = String::from_str(&s.env, "grant");
    let tid = s.client.propose_treasury_withdrawal(
        &s.members.get(0).unwrap(),
        &100,
        &dest,
        &reason,
        &false,
    );
    assert_eq!(
        s.client.get_treasury_proposal(&tid).unwrap().active_members,
        10
    );

    for i in 0..5 {
        s.client
            .vote_on_treasury_proposal(&s.members.get(i).unwrap(), &tid, &true);
    }
    s.client.exit_dao(&s.members.get(8).unwrap());
    s.client.exit_dao(&s.members.get(9).unwrap());

    // With a live count of 8 the bar would be 5 and this against-vote would
    // execute the withdrawal on five in-favour votes.
    s.client
        .vote_on_treasury_proposal(&s.members.get(5).unwrap(), &tid, &false);
    assert_eq!(
        s.client.get_treasury_proposal(&tid).unwrap().status,
        ProposalStatus::Pending
    );
    assert_eq!(s.token.balance(&dest), 0);

    s.client
        .vote_on_treasury_proposal(&s.members.get(6).unwrap(), &tid, &true);
    assert_eq!(
        s.client.get_treasury_proposal(&tid).unwrap().status,
        ProposalStatus::Executed
    );
    assert_eq!(s.token.balance(&dest), 100);
}

#[test]
fn members_joining_mid_vote_cannot_vote_on_treasury_proposal() {
    let s = setup(3);
    let dest = Address::generate(&s.env);
    let reason = String::from_str(&s.env, "grant");
    let tid = s.client.propose_treasury_withdrawal(
        &s.members.get(0).unwrap(),
        &100,
        &dest,
        &reason,
        &false,
    );
    s.client
        .vote_on_treasury_proposal(&s.members.get(0).unwrap(), &tid, &true);

    advance(&s.env, 1);
    let late = Address::generate(&s.env);
    soroban_sdk::token::StellarAssetClient::new(&s.env, &s.token.address).mint(&late, &MINT);
    s.client.register_member(&late);
    refill_treasury(&s);
    assert_eq!(s.client.get_active_members(), 5); // a live bar would now be 3

    // The late joiners are outside the electorate (checked in the shared
    // tally path, so commit-reveal reveals get the same rule).
    assert_eq!(
        s.client.try_vote_on_treasury_proposal(&late, &tid, &true),
        Err(Ok(Error::JoinedAfterProposal))
    );

    // The second original vote reaches the snapshotted bar of 2.
    s.client
        .vote_on_treasury_proposal(&s.members.get(1).unwrap(), &tid, &true);
    assert_eq!(
        s.client.get_treasury_proposal(&tid).unwrap().status,
        ProposalStatus::Executed
    );
}
