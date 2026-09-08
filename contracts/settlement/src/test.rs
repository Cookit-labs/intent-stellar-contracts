#![cfg(test)]

use super::*;
use intent_escrow::{Constraints, IntentEscrow, IntentEscrowClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{StellarAssetClient, TokenClient},
    Env,
};

const AMOUNT: i128 = 500_0000000; // 500 USDC at Stellar's 7 decimal places
const EXPECTED_OUT: i128 = 100_0000000;
const MAX_SLIPPAGE_BPS: u32 = 100; // 1%
const ONE_HOUR: u64 = 3_600;

struct Fixture {
    env: Env,
    settlement: SettlementManagerClient<'static>,
    escrow: IntentEscrowClient<'static>,
    token: TokenClient<'static>,
    depositor: Address,
    destination: Address,
    agent: Address,
    deadline: u64,
}

/// Wires settlement to a real escrow rather than a stub: the release path is
/// the part most worth proving, and a stub that always succeeds would prove
/// nothing about it.
fn setup() -> Fixture {
    let env = Env::default();
    env.mock_all_auths();

    let issuer = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(issuer);
    let token = TokenClient::new(&env, &sac.address());
    let token_admin = StellarAssetClient::new(&env, &sac.address());

    let depositor = Address::generate(&env);
    let destination = Address::generate(&env);
    let agent = Address::generate(&env);
    let validator = Address::generate(&env);

    let escrow_id = env.register(IntentEscrow, ());
    let settlement_id = env.register(SettlementManager, ());

    let escrow = IntentEscrowClient::new(&env, &escrow_id);
    escrow.initialise(&settlement_id);

    let settlement = SettlementManagerClient::new(&env, &settlement_id);
    settlement.initialise(&escrow_id, &validator);

    token_admin.mint(&depositor, &(AMOUNT * 10));

    let deadline = env.ledger().timestamp() + ONE_HOUR;

    Fixture {
        env,
        settlement,
        escrow,
        token,
        depositor,
        destination,
        agent,
        deadline,
    }
}

impl Fixture {
    fn fund(&self, intent_id: &str) -> soroban_sdk::String {
        let id = soroban_sdk::String::from_str(&self.env, intent_id);
        self.escrow.deposit(
            &id,
            &self.depositor,
            &self.token.address,
            &AMOUNT,
            &Constraints {
                token_in: soroban_sdk::String::from_str(&self.env, "USDC"),
                token_out: soroban_sdk::String::from_str(&self.env, "XLM"),
                max_slippage_bps: MAX_SLIPPAGE_BPS,
                deadline: self.deadline,
            },
        );
        id
    }

    fn report(&self, amount_out: i128) -> ExecutionReport {
        ExecutionReport {
            agent: self.agent.clone(),
            destination: self.destination.clone(),
            amount_in: AMOUNT,
            amount_out,
            expected_out: EXPECTED_OUT,
        }
    }
}

#[test]
fn settles_a_clean_execution_and_pays_the_destination() {
    let f = setup();
    let id = f.fund("intent_1");

    let before = f.token.balance(&f.destination);
    let slippage = f
        .settlement
        .settle(&id, &f.report(EXPECTED_OUT), &MAX_SLIPPAGE_BPS, &f.deadline, &AMOUNT);

    assert_eq!(slippage, 0, "a fill at the expected price has no slippage");
    assert_eq!(
        f.token.balance(&f.destination) - before,
        AMOUNT,
        "the escrowed amount should reach the destination"
    );
}

#[test]
fn records_the_outcome_for_reputation() {
    let f = setup();
    let id = f.fund("intent_1");
    f.settlement
        .settle(&id, &f.report(EXPECTED_OUT), &MAX_SLIPPAGE_BPS, &f.deadline, &AMOUNT);

    let record = f.settlement.get_settlement(&id);
    assert_eq!(record.agent, f.agent);
    assert_eq!(record.amount_in, AMOUNT);
    assert_eq!(record.amount_out, EXPECTED_OUT);
    assert_eq!(record.slippage_bps, 0);
}

#[test]
fn derives_slippage_rather_than_trusting_the_agent() {
    let f = setup();
    let id = f.fund("intent_1");

    // 1% short of the expected fill.
    let delivered = EXPECTED_OUT - EXPECTED_OUT / 100;
    let slippage = f
        .settlement
        .settle(&id, &f.report(delivered), &MAX_SLIPPAGE_BPS, &f.deadline, &AMOUNT);

    assert_eq!(slippage, 100, "a 1% shortfall is 100 bps");
}

#[test]
fn rejects_an_execution_worse_than_the_tolerance() {
    let f = setup();
    let id = f.fund("intent_1");

    // 5% short against a 1% tolerance.
    let delivered = EXPECTED_OUT - EXPECTED_OUT / 20;
    let result =
        f.settlement
            .try_settle(&id, &f.report(delivered), &MAX_SLIPPAGE_BPS, &f.deadline, &AMOUNT);

    assert_eq!(result, Err(Ok(Error::SlippageExceeded)));
}

#[test]
fn treats_a_better_fill_as_zero_slippage() {
    let f = setup();
    let id = f.fund("intent_1");

    // Delivering more than expected is a better fill, not negative slippage.
    let slippage = f.settlement.settle(
        &id,
        &f.report(EXPECTED_OUT * 2),
        &MAX_SLIPPAGE_BPS,
        &f.deadline,
        &AMOUNT,
    );

    assert_eq!(slippage, 0);
}

#[test]
fn refuses_to_settle_the_same_intent_twice() {
    let f = setup();
    let id = f.fund("intent_1");
    f.settlement
        .settle(&id, &f.report(EXPECTED_OUT), &MAX_SLIPPAGE_BPS, &f.deadline, &AMOUNT);

    let again =
        f.settlement
            .try_settle(&id, &f.report(EXPECTED_OUT), &MAX_SLIPPAGE_BPS, &f.deadline, &AMOUNT);

    assert_eq!(again, Err(Ok(Error::AlreadySettled)));
}

#[test]
fn rejects_a_spend_larger_than_the_escrow_holds() {
    let f = setup();
    let id = f.fund("intent_1");

    let mut report = f.report(EXPECTED_OUT);
    report.amount_in = AMOUNT * 2;

    let result = f
        .settlement
        .try_settle(&id, &report, &MAX_SLIPPAGE_BPS, &f.deadline, &AMOUNT);

    assert_eq!(result, Err(Ok(Error::AmountExceedsEscrow)));
}

#[test]
fn rejects_an_execution_reported_after_the_deadline() {
    let f = setup();
    let id = f.fund("intent_1");

    f.env.ledger().with_mut(|l| l.timestamp += ONE_HOUR * 2);

    let result =
        f.settlement
            .try_settle(&id, &f.report(EXPECTED_OUT), &MAX_SLIPPAGE_BPS, &f.deadline, &AMOUNT);

    assert_eq!(result, Err(Ok(Error::DeadlinePassed)));
}

#[test]
fn rejects_zero_amounts() {
    let f = setup();
    let id = f.fund("intent_1");

    let mut report = f.report(EXPECTED_OUT);
    report.amount_out = 0;

    assert_eq!(
        f.settlement
            .try_settle(&id, &report, &MAX_SLIPPAGE_BPS, &f.deadline, &AMOUNT),
        Err(Ok(Error::ZeroAmount))
    );
}

#[test]
fn rejects_an_unusable_expected_out() {
    let f = setup();
    let id = f.fund("intent_1");

    let mut report = f.report(EXPECTED_OUT);
    report.expected_out = 0;

    // Slippage cannot be derived from a zero baseline; refusing beats dividing.
    assert_eq!(
        f.settlement
            .try_settle(&id, &report, &MAX_SLIPPAGE_BPS, &f.deadline, &AMOUNT),
        Err(Ok(Error::InvalidExpectedOut))
    );
}

#[test]
fn does_not_round_small_shortfalls_away() {
    let f = setup();
    let id = f.fund("intent_1");

    // 0.1% short. Dividing before scaling would report this as zero.
    let delivered = EXPECTED_OUT - EXPECTED_OUT / 1000;
    let slippage = f
        .settlement
        .settle(&id, &f.report(delivered), &MAX_SLIPPAGE_BPS, &f.deadline, &AMOUNT);

    assert_eq!(slippage, 10, "0.1% is 10 bps");
}

#[test]
fn cannot_be_initialised_twice() {
    let f = setup();
    let other = Address::generate(&f.env);

    assert_eq!(
        f.settlement.try_initialise(&other, &other),
        Err(Ok(Error::AlreadyInitialised))
    );
}

#[test]
fn reports_an_unknown_settlement_rather_than_panicking() {
    let f = setup();
    let missing = soroban_sdk::String::from_str(&f.env, "never_settled");

    assert_eq!(
        f.settlement.try_get_settlement(&missing),
        Err(Ok(Error::SettlementNotFound))
    );
}
