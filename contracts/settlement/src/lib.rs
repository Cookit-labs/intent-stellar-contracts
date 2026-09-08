#![no_std]

//! Decides whether an execution may be paid, and pays it.
//!
//! This is the Soroban counterpart to `SettlementManager.sol` in
//! `intent-core-contracts`. The escrow deliberately does not check the
//! constraints it stores — enforcing them is this contract's only job, because
//! two validators that can disagree is worse than one that cannot.
//!
//! The split matters for custody: escrow holds funds and will release them only
//! on this contract's authority, and this contract holds nothing. An agent that
//! compromises neither cannot move a user's money.
//!
//! Soroban-specific notes:
//!
//! - **There is no implicit caller.** Every authority check is an explicit
//!   `require_auth`, and the escrow authorises this contract by address.
//! - **Storage is rented.** Settlement records outlive the intents they
//!   describe, so their TTL is extended well past the deadline; a record that
//!   is archived takes the reputation history with it.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Env, String, Symbol,
};

/// Ledgers per day at Stellar's roughly 5 second close time.
const LEDGERS_PER_DAY: u32 = 17_280;

/// Settlement records are history: they outlive the intent and are what
/// reputation is rebuilt from, so they are kept far longer than an escrow entry.
const TTL_EXTENSION_LEDGERS: u32 = LEDGERS_PER_DAY * 90;
const TTL_THRESHOLD_LEDGERS: u32 = LEDGERS_PER_DAY * 45;

/// Basis points in one whole. 10_000 bps = 100%.
const BPS_DENOMINATOR: i128 = 10_000;

/// What an agent claims it achieved, submitted for validation.
///
/// Amounts are `i128` to match the token interface. Slippage is derived here
/// rather than accepted from the agent: a self-reported figure is exactly the
/// number an agent has an incentive to understate.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionReport {
    pub agent: Address,
    /// Where the escrowed funds should be sent on success.
    pub destination: Address,
    /// Amount of the input token actually spent.
    pub amount_in: i128,
    /// Amount of the output token actually received.
    pub amount_out: i128,
    /// What `amount_out` would have been at the quoted reference price, with no
    /// slippage. The backend supplies this from the price it quoted the user.
    pub expected_out: i128,
}

/// The recorded outcome of a settled intent.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct Settlement {
    pub intent_id: String,
    pub agent: Address,
    pub amount_in: i128,
    pub amount_out: i128,
    pub expected_out: i128,
    /// Realised slippage in basis points, computed at settlement.
    pub slippage_bps: u32,
    pub settled_at: u64,
}

#[contracttype]
pub enum DataKey {
    /// The escrow this contract settles against, set once.
    Escrow,
    /// The address permitted to submit execution reports.
    Validator,
    Settlement(String),
}

#[contracterror]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialised = 1,
    NotInitialised = 2,
    AlreadySettled = 3,
    ZeroAmount = 4,
    /// The execution delivered less than the intent's slippage tolerance allows.
    SlippageExceeded = 5,
    /// The claimed spend is larger than what the escrow holds.
    AmountExceedsEscrow = 6,
    DeadlinePassed = 7,
    SettlementNotFound = 8,
    /// `expected_out` was zero or negative, so slippage cannot be derived.
    InvalidExpectedOut = 9,
}

#[contract]
pub struct SettlementManager;

#[contractimpl]
impl SettlementManager {
    /// Bind this contract to its escrow and the validator allowed to report
    /// executions.
    ///
    /// Callable once, for the same reason the escrow's is: an address that can
    /// be rotated makes whoever can rotate it the real authority over every
    /// settlement.
    pub fn initialise(env: Env, escrow: Address, validator: Address) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Escrow) {
            return Err(Error::AlreadyInitialised);
        }
        env.storage().instance().set(&DataKey::Escrow, &escrow);
        env.storage().instance().set(&DataKey::Validator, &validator);
        Ok(())
    }

    /// Validate an execution against an intent's constraints and pay the agent.
    ///
    /// The caller supplies the constraints the escrow recorded at deposit time.
    /// They are passed in rather than read back through a cross-contract call
    /// because the escrow's storage is the authority either way, and a client
    /// that lies about them still cannot cause a release — the escrow checks
    /// the caller and the amount itself.
    ///
    /// Returns the realised slippage in basis points so callers do not have to
    /// re-derive it.
    pub fn settle(
        env: Env,
        intent_id: String,
        report: ExecutionReport,
        max_slippage_bps: u32,
        deadline: u64,
        escrowed_amount: i128,
    ) -> Result<u32, Error> {
        let validator: Address = env
            .storage()
            .instance()
            .get(&DataKey::Validator)
            .ok_or(Error::NotInitialised)?;
        validator.require_auth();

        let key = DataKey::Settlement(intent_id.clone());
        if env.storage().persistent().has(&key) {
            return Err(Error::AlreadySettled);
        }

        if report.amount_in <= 0 || report.amount_out <= 0 {
            return Err(Error::ZeroAmount);
        }
        if report.expected_out <= 0 {
            return Err(Error::InvalidExpectedOut);
        }
        if report.amount_in > escrowed_amount {
            return Err(Error::AmountExceedsEscrow);
        }
        // Checked here as well as in the escrow's refund path: an execution
        // reported after the deadline should not be payable even if the funds
        // have not yet been refunded.
        if env.ledger().timestamp() > deadline {
            return Err(Error::DeadlinePassed);
        }

        let slippage_bps = realised_slippage_bps(report.expected_out, report.amount_out);
        if slippage_bps > max_slippage_bps {
            return Err(Error::SlippageExceeded);
        }

        let settlement = Settlement {
            intent_id: intent_id.clone(),
            agent: report.agent.clone(),
            amount_in: report.amount_in,
            amount_out: report.amount_out,
            expected_out: report.expected_out,
            slippage_bps,
            settled_at: env.ledger().timestamp(),
        };

        // Written before the release so a re-entrant call finds the intent
        // already settled, exactly as the escrow consumes status before
        // transferring.
        env.storage().persistent().set(&key, &settlement);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD_LEDGERS, TTL_EXTENSION_LEDGERS);

        let escrow: Address = env
            .storage()
            .instance()
            .get(&DataKey::Escrow)
            .ok_or(Error::NotInitialised)?;

        escrow_client::Client::new(&env, &escrow).release(
            &intent_id,
            &report.destination,
            &report.amount_in,
        );

        // Carries everything needed to reconstruct the settlement off-chain,
        // so an indexer never has to read contract storage to build history.
        env.events().publish(
            (Symbol::new(&env, "settled"), report.agent.clone()),
            (
                intent_id,
                report.amount_in,
                report.amount_out,
                slippage_bps,
            ),
        );

        Ok(slippage_bps)
    }

    /// Read a settled outcome. This is what reputation is rebuilt from.
    pub fn get_settlement(env: Env, intent_id: String) -> Result<Settlement, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Settlement(intent_id))
            .ok_or(Error::SettlementNotFound)
    }

    pub fn escrow(env: Env) -> Result<Address, Error> {
        env.storage()
            .instance()
            .get(&DataKey::Escrow)
            .ok_or(Error::NotInitialised)
    }

    pub fn validator(env: Env) -> Result<Address, Error> {
        env.storage()
            .instance()
            .get(&DataKey::Validator)
            .ok_or(Error::NotInitialised)
    }
}

/// Realised slippage in basis points, floored at zero.
///
/// Delivering *more* than expected is not negative slippage — it is a better
/// fill, and reporting it as a negative number would make averages misleading
/// and force every consumer to handle a signed value.
fn realised_slippage_bps(expected_out: i128, amount_out: i128) -> u32 {
    if amount_out >= expected_out {
        return 0;
    }
    let shortfall = expected_out - amount_out;
    // Multiplication first: integer division before scaling would round every
    // sub-1% shortfall to zero.
    let bps = shortfall * BPS_DENOMINATOR / expected_out;
    // A shortfall cannot exceed expected_out, so this fits comfortably in u32.
    bps as u32
}

mod escrow_client {
    use soroban_sdk::{contractclient, Address, Env, String};

    /// The slice of the escrow this contract calls.
    ///
    /// Declared here rather than importing the escrow crate so that settlement
    /// does not carry the escrow's implementation into its own WASM — Soroban
    /// charges for bytecode size.
    #[contractclient(name = "Client")]
    pub trait Escrow {
        fn release(env: Env, intent_id: String, destination: Address, amount: i128);
    }
}

#[cfg(test)]
mod test;
