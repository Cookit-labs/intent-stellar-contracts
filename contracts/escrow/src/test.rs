#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{StellarAssetClient, TokenClient},
    Env, String,
};

const AMOUNT: i128 = 500_0000000; // 500 USDC at Stellar's 7 decimal places
const MAX_SLIPPAGE_BPS: u32 = 100;
const ONE_HOUR: u64 = 3_600;

struct Fixture {
    env: Env,
    escrow: IntentEscrowClient<'static>,
    token: TokenClient<'static>,
    token_admin: StellarAssetClient<'static>,
    settlement: Address,
    depositor: Address,
    destination: Address,
}

fn setup() -> Fixture {
    let env = Env::default();
    env.mock_all_auths();

    let issuer = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(issuer);
    let token = TokenClient::new(&env, &sac.address());
    let token_admin = StellarAssetClient::new(&env, &sac.address());

    let settlement = Address::generate(&env);
    let depositor = Address::generate(&env);
    let destination = Address::generate(&env);

    let escrow_id = env.register(IntentEscrow, ());
    let escrow = IntentEscrowClient::new(&env, &escrow_id);
    escrow.initialise(&settlement);

    token_admin.mint(&depositor, &(AMOUNT * 10));

    Fixture {
        env,
        escrow,
        token,
        token_admin,
        settlement,
        depositor,
        destination,
    }
}

fn constraints(env: &Env) -> Constraints {
    Constraints {
        token_in: String::from_str(env, "USDC"),
        token_out: String::from_str(env, "XLM"),
        max_slippage_bps: MAX_SLIPPAGE_BPS,
        deadline: env.ledger().timestamp() + ONE_HOUR,
    }
}

fn intent_id(env: &Env) -> String {
    String::from_str(env, "intent-1")
}

// --- deposit ---------------------------------------------------------------

#[test]
fn deposit_moves_tokens_into_the_escrow() {
    let f = setup();
    let id = intent_id(&f.env);

    f.escrow.deposit(
        &id,
        &f.depositor,
        &f.token.address,
        &AMOUNT,
        &constraints(&f.env),
    );

    assert_eq!(f.token.balance(&f.escrow.address), AMOUNT);
    assert_eq!(f.token.balance(&f.depositor), AMOUNT * 9);
}

#[test]
fn deposit_records_the_depositor_and_constraints() {
    let f = setup();
    let id = intent_id(&f.env);

    f.escrow.deposit(
        &id,
        &f.depositor,
        &f.token.address,
        &AMOUNT,
        &constraints(&f.env),
    );

    let intent = f.escrow.get_intent(&id);
    assert_eq!(intent.depositor, f.depositor);
    assert_eq!(intent.amount, AMOUNT);
    assert_eq!(intent.status, Status::Funded);
    assert_eq!(intent.constraints.max_slippage_bps, MAX_SLIPPAGE_BPS);
}

#[test]
fn deposit_rejects_a_non_positive_amount() {
    let f = setup();
    let id = intent_id(&f.env);

    let err = f
        .escrow
        .try_deposit(
            &id,
            &f.depositor,
            &f.token.address,
            &0,
            &constraints(&f.env),
        )
        .unwrap_err()
        .unwrap();
    assert_eq!(err, Error::ZeroAmount);

    let err = f
        .escrow
        .try_deposit(
            &id,
            &f.depositor,
            &f.token.address,
            &-1,
            &constraints(&f.env),
        )
        .unwrap_err()
        .unwrap();
    assert_eq!(err, Error::ZeroAmount);
}

#[test]
fn deposit_rejects_a_deadline_in_the_past() {
    let f = setup();
    let id = intent_id(&f.env);

    let mut c = constraints(&f.env);
    c.deadline = f.env.ledger().timestamp();

    let err = f
        .escrow
        .try_deposit(&id, &f.depositor, &f.token.address, &AMOUNT, &c)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, Error::DeadlineInThePast);
}

/// Reusing an intent id would let a second deposit overwrite the first
/// depositor's claim on funds already held.
#[test]
fn deposit_rejects_a_duplicate_intent_id() {
    let f = setup();
    let id = intent_id(&f.env);
    let c = constraints(&f.env);

    f.escrow
        .deposit(&id, &f.depositor, &f.token.address, &AMOUNT, &c);

    let err = f
        .escrow
        .try_deposit(&id, &f.depositor, &f.token.address, &AMOUNT, &c)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, Error::IntentAlreadyExists);
}

// --- release ---------------------------------------------------------------

#[test]
fn release_sends_funds_to_the_destination() {
    let f = setup();
    let id = intent_id(&f.env);

    f.escrow.deposit(
        &id,
        &f.depositor,
        &f.token.address,
        &AMOUNT,
        &constraints(&f.env),
    );
    f.escrow.release(&id, &f.destination, &AMOUNT);

    assert_eq!(f.token.balance(&f.destination), AMOUNT);
    assert_eq!(f.token.balance(&f.escrow.address), 0);
}

#[test]
fn release_rejects_more_than_was_escrowed() {
    let f = setup();
    let id = intent_id(&f.env);

    f.escrow.deposit(
        &id,
        &f.depositor,
        &f.token.address,
        &AMOUNT,
        &constraints(&f.env),
    );

    let err = f
        .escrow
        .try_release(&id, &f.destination, &(AMOUNT + 1))
        .unwrap_err()
        .unwrap();
    assert_eq!(err, Error::AmountExceedsEscrow);
    assert_eq!(f.token.balance(&f.escrow.address), AMOUNT);
}

#[test]
fn release_cannot_happen_twice() {
    let f = setup();
    let id = intent_id(&f.env);

    f.escrow.deposit(
        &id,
        &f.depositor,
        &f.token.address,
        &AMOUNT,
        &constraints(&f.env),
    );
    f.escrow.release(&id, &f.destination, &AMOUNT);

    let err = f
        .escrow
        .try_release(&id, &f.destination, &AMOUNT)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, Error::IntentNotFunded);
}

#[test]
fn release_rejects_an_unknown_intent() {
    let f = setup();
    let unknown = String::from_str(&f.env, "never-deposited");

    let err = f
        .escrow
        .try_release(&unknown, &f.destination, &AMOUNT)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, Error::IntentNotFunded);
}

// --- refund ----------------------------------------------------------------

/// Permissionless by design. If a user must be online to reclaim their own
/// money, the escrow is a liability rather than a protection.
#[test]
fn anyone_can_trigger_refund_after_the_deadline() {
    let f = setup();
    let id = intent_id(&f.env);
    let before = f.token.balance(&f.depositor);

    f.escrow.deposit(
        &id,
        &f.depositor,
        &f.token.address,
        &AMOUNT,
        &constraints(&f.env),
    );

    f.env.ledger().with_mut(|l| l.timestamp += ONE_HOUR * 2);
    f.escrow.refund(&id);

    assert_eq!(f.token.balance(&f.depositor), before);
    assert_eq!(f.token.balance(&f.escrow.address), 0);
}

/// Funds go to the recorded depositor regardless of who calls, so a
/// permissionless refund cannot be used to redirect money.
#[test]
fn refund_always_returns_to_the_depositor() {
    let f = setup();
    let id = intent_id(&f.env);
    let stranger = Address::generate(&f.env);

    f.escrow.deposit(
        &id,
        &f.depositor,
        &f.token.address,
        &AMOUNT,
        &constraints(&f.env),
    );

    f.env.ledger().with_mut(|l| l.timestamp += ONE_HOUR * 2);
    f.escrow.refund(&id);

    assert_eq!(f.token.balance(&stranger), 0);
    assert_eq!(f.token.balance(&f.escrow.address), 0);
}

#[test]
fn refund_rejected_before_the_deadline() {
    let f = setup();
    let id = intent_id(&f.env);

    f.escrow.deposit(
        &id,
        &f.depositor,
        &f.token.address,
        &AMOUNT,
        &constraints(&f.env),
    );

    let err = f.escrow.try_refund(&id).unwrap_err().unwrap();
    assert_eq!(err, Error::DeadlineNotPassed);
}

/// The boundary itself: refund is available strictly after the deadline, so it
/// cannot race a settlement that is still valid.
#[test]
fn refund_rejected_at_exactly_the_deadline() {
    let f = setup();
    let id = intent_id(&f.env);
    let c = constraints(&f.env);
    let deadline = c.deadline;

    f.escrow
        .deposit(&id, &f.depositor, &f.token.address, &AMOUNT, &c);

    f.env.ledger().with_mut(|l| l.timestamp = deadline);

    let err = f.escrow.try_refund(&id).unwrap_err().unwrap();
    assert_eq!(err, Error::DeadlineNotPassed);
}

#[test]
fn refund_succeeds_one_second_after_the_deadline() {
    let f = setup();
    let id = intent_id(&f.env);
    let c = constraints(&f.env);
    let deadline = c.deadline;

    f.escrow
        .deposit(&id, &f.depositor, &f.token.address, &AMOUNT, &c);

    f.env.ledger().with_mut(|l| l.timestamp = deadline + 1);
    f.escrow.refund(&id);

    assert_eq!(f.token.balance(&f.escrow.address), 0);
}

#[test]
fn refund_cannot_happen_twice() {
    let f = setup();
    let id = intent_id(&f.env);

    f.escrow.deposit(
        &id,
        &f.depositor,
        &f.token.address,
        &AMOUNT,
        &constraints(&f.env),
    );

    f.env.ledger().with_mut(|l| l.timestamp += ONE_HOUR * 2);
    f.escrow.refund(&id);

    let err = f.escrow.try_refund(&id).unwrap_err().unwrap();
    assert_eq!(err, Error::IntentNotFunded);
}

#[test]
fn refund_rejected_after_release() {
    let f = setup();
    let id = intent_id(&f.env);

    f.escrow.deposit(
        &id,
        &f.depositor,
        &f.token.address,
        &AMOUNT,
        &constraints(&f.env),
    );
    f.escrow.release(&id, &f.destination, &AMOUNT);

    f.env.ledger().with_mut(|l| l.timestamp += ONE_HOUR * 2);

    let err = f.escrow.try_refund(&id).unwrap_err().unwrap();
    assert_eq!(err, Error::IntentNotFunded);
}

#[test]
fn release_rejected_after_refund() {
    let f = setup();
    let id = intent_id(&f.env);

    f.escrow.deposit(
        &id,
        &f.depositor,
        &f.token.address,
        &AMOUNT,
        &constraints(&f.env),
    );

    f.env.ledger().with_mut(|l| l.timestamp += ONE_HOUR * 2);
    f.escrow.refund(&id);

    let err = f
        .escrow
        .try_release(&id, &f.destination, &AMOUNT)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, Error::IntentNotFunded);
}

// --- initialisation --------------------------------------------------------

/// A rotatable settlement address would let whoever can rotate it redirect
/// every escrow in the contract.
#[test]
fn initialise_cannot_be_called_twice() {
    let f = setup();
    let other = Address::generate(&f.env);

    let err = f.escrow.try_initialise(&other).unwrap_err().unwrap();
    assert_eq!(err, Error::AlreadyInitialised);
    assert_eq!(f.escrow.settlement(), f.settlement);
}

// --- accounting ------------------------------------------------------------

/// Two intents must not be able to spend each other's funds.
#[test]
fn intents_are_isolated() {
    let f = setup();
    let first = intent_id(&f.env);
    let second = String::from_str(&f.env, "intent-2");
    let c = constraints(&f.env);

    f.escrow
        .deposit(&first, &f.depositor, &f.token.address, &AMOUNT, &c);
    f.escrow
        .deposit(&second, &f.depositor, &f.token.address, &AMOUNT, &c);

    f.escrow.release(&first, &f.destination, &AMOUNT);

    let remaining = f.escrow.get_intent(&second);
    assert_eq!(remaining.amount, AMOUNT);
    assert_eq!(remaining.status, Status::Funded);
    assert_eq!(f.token.balance(&f.escrow.address), AMOUNT);
}

/// No sequence of operations may leave the contract holding value that nobody
/// can withdraw.
#[test]
fn funds_are_never_stranded() {
    let f = setup();
    let c = constraints(&f.env);

    for (i, release_it) in [true, false].iter().enumerate() {
        let id = String::from_str(&f.env, if i == 0 { "strand-a" } else { "strand-b" });
        f.escrow
            .deposit(&id, &f.depositor, &f.token.address, &AMOUNT, &c);

        if *release_it {
            f.escrow.release(&id, &f.destination, &AMOUNT);
        } else {
            f.env.ledger().with_mut(|l| l.timestamp += ONE_HOUR * 2);
            f.escrow.refund(&id);
        }
    }

    assert_eq!(f.token.balance(&f.escrow.address), 0);
}

/// Unused in assertions, but proves the fixture mints through the real Stellar
/// asset contract rather than a mock that might diverge from live behaviour.
#[test]
fn token_admin_can_mint() {
    let f = setup();
    let other = Address::generate(&f.env);

    f.token_admin.mint(&other, &1_000);
    assert_eq!(f.token.balance(&other), 1_000);
}

// --- authorisation ---------------------------------------------------------

/// The security boundary of the whole system: only the settlement contract may
/// move escrowed funds.
///
/// The other tests run under `mock_all_auths`, which authorises everything and
/// therefore proves nothing about who may call what. This one builds an env
/// without that blanket mock, so a release attempted without the settlement
/// contract's authorisation genuinely fails.
#[test]
#[should_panic]
fn release_without_settlement_auth_is_rejected() {
    let env = Env::default();
    env.mock_all_auths();

    let issuer = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(issuer);
    let token = TokenClient::new(&env, &sac.address());
    let token_admin = StellarAssetClient::new(&env, &sac.address());

    let settlement = Address::generate(&env);
    let depositor = Address::generate(&env);
    let destination = Address::generate(&env);

    let escrow_id = env.register(IntentEscrow, ());
    let escrow = IntentEscrowClient::new(&env, &escrow_id);
    escrow.initialise(&settlement);
    token_admin.mint(&depositor, &AMOUNT);

    let id = String::from_str(&env, "auth-test");
    let c = Constraints {
        token_in: String::from_str(&env, "USDC"),
        token_out: String::from_str(&env, "XLM"),
        max_slippage_bps: MAX_SLIPPAGE_BPS,
        deadline: env.ledger().timestamp() + ONE_HOUR,
    };
    escrow.deposit(&id, &depositor, &token.address, &AMOUNT, &c);

    // Stop authorising everything. The release below carries no authorisation
    // from the settlement contract, so require_auth must reject it.
    env.set_auths(&[]);
    escrow.release(&id, &destination, &AMOUNT);
}

/// Same for deposit: naming someone else as depositor must not move their
/// tokens without their authorisation.
#[test]
#[should_panic]
fn deposit_without_depositor_auth_is_rejected() {
    let env = Env::default();
    env.mock_all_auths();

    let issuer = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(issuer);
    let token = TokenClient::new(&env, &sac.address());
    let token_admin = StellarAssetClient::new(&env, &sac.address());

    let settlement = Address::generate(&env);
    let victim = Address::generate(&env);

    let escrow_id = env.register(IntentEscrow, ());
    let escrow = IntentEscrowClient::new(&env, &escrow_id);
    escrow.initialise(&settlement);
    token_admin.mint(&victim, &AMOUNT);

    let id = String::from_str(&env, "auth-deposit");
    let c = Constraints {
        token_in: String::from_str(&env, "USDC"),
        token_out: String::from_str(&env, "XLM"),
        max_slippage_bps: MAX_SLIPPAGE_BPS,
        deadline: env.ledger().timestamp() + ONE_HOUR,
    };

    env.set_auths(&[]);
    escrow.deposit(&id, &victim, &token.address, &AMOUNT, &c);
}
