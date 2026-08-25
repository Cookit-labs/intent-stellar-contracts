#![no_std]

//! Holds a user's USDC for the life of an intent.
//!
//! This is the Soroban counterpart to `IntentEscrow.sol` in
//! `intent-core-contracts`, and it exists for the same reason: to make "agents
//! never custody user funds" a property rather than a promise. From deposit
//! until settlement, capital sits here. Agents never hold it and cannot move
//! it — releasing requires the settlement contract, which validates the
//! proposed execution against the constraints stored below.
//!
//! Two differences from the Arc implementation are worth knowing, because they
//! come from Soroban's model rather than from a design choice:
//!
//! - **Value is a token balance, not a native transfer.** Stellar has no
//!   `msg.value`; funds move through a token client, and the depositor must
//!   authorise the call for the contract to pull them.
//! - **Storage is explicit and rented.** Entries must have their time-to-live
//!   extended or they are archived. An escrow whose entry expired while still
//!   holding funds would be unrecoverable, so deposits extend TTL well past
//!   the refund deadline.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Env, String,
};

/// Ledgers per day, at Stellar's roughly 5 second close time.
const LEDGERS_PER_DAY: u32 = 17_280;

/// How long an intent's storage entry outlives its deadline. Funds must remain
/// refundable after expiry, so the entry cannot be archived first.
const TTL_EXTENSION_LEDGERS: u32 = LEDGERS_PER_DAY * 30;
const TTL_THRESHOLD_LEDGERS: u32 = LEDGERS_PER_DAY * 15;

#[contracttype]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    /// Holding funds, awaiting settlement or expiry.
    Funded = 0,
    /// Settled — funds released to the execution destination.
    Released = 1,
    /// Expired — funds returned to the depositor.
    Refunded = 2,
}

/// The bounds an execution must satisfy.
///
/// Stored on deposit and read by the settlement contract. This contract does
/// not interpret them; enforcing them is settlement's job, and keeping that
/// split means escrow has one responsibility: custody.
#[contracttype]
#[derive(Clone)]
pub struct Constraints {
    pub token_in: String,
    pub token_out: String,
    /// Maximum acceptable slippage in basis points. 100 = 1%.
    pub max_slippage_bps: u32,
    /// Unix seconds. After this, anyone may trigger a refund.
    pub deadline: u64,
}

#[contracttype]
#[derive(Clone)]
pub struct Intent {
    pub depositor: Address,
    pub token: Address,
    pub amount: i128,
    pub constraints: Constraints,
    pub status: Status,
}

#[contracttype]
pub enum DataKey {
    /// The settlement contract, set once at initialisation.
    Settlement,
    Intent(String),
}

#[contracterror]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialised = 1,
    NotInitialised = 2,
    ZeroAmount = 3,
    DeadlineInThePast = 4,
    DeadlineNotPassed = 5,
    IntentAlreadyExists = 6,
    IntentNotFunded = 7,
    AmountExceedsEscrow = 8,
}

#[contract]
pub struct IntentEscrow;

#[contractimpl]
impl IntentEscrow {
    /// Bind the escrow to its settlement contract.
    ///
    /// Callable once. A rotatable settlement address would let whoever can
    /// rotate it redirect every escrow, which would make them the real
    /// custodian.
    pub fn initialise(env: Env, settlement: Address) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Settlement) {
            return Err(Error::AlreadyInitialised);
        }
        env.storage()
            .instance()
            .set(&DataKey::Settlement, &settlement);
        Ok(())
    }

    /// Lock tokens against an intent, with the bounds any execution must
    /// respect.
    ///
    /// `intent_id` is minted by the backend. Reuse is rejected, so a second
    /// deposit cannot overwrite an existing depositor's claim on funds.
    pub fn deposit(
        env: Env,
        intent_id: String,
        depositor: Address,
        token: Address,
        amount: i128,
        constraints: Constraints,
    ) -> Result<(), Error> {
        // Soroban has no implicit caller. Without this, anyone could move
        // anyone else's tokens by naming them as depositor.
        depositor.require_auth();

        if amount <= 0 {
            return Err(Error::ZeroAmount);
        }
        if constraints.deadline <= env.ledger().timestamp() {
            return Err(Error::DeadlineInThePast);
        }

        let key = DataKey::Intent(intent_id);
        if env.storage().persistent().has(&key) {
            return Err(Error::IntentAlreadyExists);
        }

        token::Client::new(&env, &token).transfer(
            &depositor,
            env.current_contract_address(),
            &amount,
        );

        let intent = Intent {
            depositor,
            token,
            amount,
            constraints,
            status: Status::Funded,
        };
        env.storage().persistent().set(&key, &intent);

        // Without this the entry can be archived while still holding funds,
        // which would strand them.
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD_LEDGERS, TTL_EXTENSION_LEDGERS);

        Ok(())
    }

    /// Release escrowed funds to an execution destination.
    ///
    /// Callable only by the settlement contract, which is responsible for
    /// having validated the execution against `constraints` first. This
    /// contract deliberately does not re-check them: two validators that can
    /// disagree is worse than one that cannot.
    pub fn release(
        env: Env,
        intent_id: String,
        destination: Address,
        amount: i128,
    ) -> Result<(), Error> {
        let settlement: Address = env
            .storage()
            .instance()
            .get(&DataKey::Settlement)
            .ok_or(Error::NotInitialised)?;
        settlement.require_auth();

        let key = DataKey::Intent(intent_id);
        let mut intent: Intent = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::IntentNotFunded)?;

        if intent.status != Status::Funded {
            return Err(Error::IntentNotFunded);
        }
        if amount > intent.amount {
            return Err(Error::AmountExceedsEscrow);
        }

        // Consumed before the transfer, so a re-entrant call finds an intent
        // that is no longer Funded.
        intent.status = Status::Released;
        env.storage().persistent().set(&key, &intent);

        token::Client::new(&env, &intent.token).transfer(
            &env.current_contract_address(),
            &destination,
            &amount,
        );

        Ok(())
    }

    /// Return escrowed funds to the depositor after the deadline.
    ///
    /// Permissionless on purpose. Funds always go to the recorded depositor
    /// rather than the caller, so opening this up cannot redirect money — and a
    /// user who is offline, or has lost their key, does not lose their deposit.
    pub fn refund(env: Env, intent_id: String) -> Result<(), Error> {
        let key = DataKey::Intent(intent_id);
        let mut intent: Intent = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::IntentNotFunded)?;

        if intent.status != Status::Funded {
            return Err(Error::IntentNotFunded);
        }
        if env.ledger().timestamp() <= intent.constraints.deadline {
            return Err(Error::DeadlineNotPassed);
        }

        intent.status = Status::Refunded;
        env.storage().persistent().set(&key, &intent);

        token::Client::new(&env, &intent.token).transfer(
            &env.current_contract_address(),
            &intent.depositor,
            &intent.amount,
        );

        Ok(())
    }

    /// Read an intent, including the constraints settlement must enforce.
    pub fn get_intent(env: Env, intent_id: String) -> Result<Intent, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Intent(intent_id))
            .ok_or(Error::IntentNotFunded)
    }

    pub fn settlement(env: Env) -> Result<Address, Error> {
        env.storage()
            .instance()
            .get(&DataKey::Settlement)
            .ok_or(Error::NotInitialised)
    }
}

#[cfg(test)]
mod test;
