#![no_std]

//! Agent identity and reputation.
//!
//! The product claim is that agents carry persistent reputation, which is what
//! separates this from a stateless solver auction where losing costs nothing.
//! That claim only holds if the numbers come from measured outcomes rather than
//! from the agents themselves, so **only the settlement contract may write
//! here**. An agent that could update its own record would make the leaderboard
//! meaningless.
//!
//! Two Soroban-shaped decisions:
//!
//! - **Counters are kept in one compact record per agent, updated in place.**
//!   Soroban charges for state, so an append-only log of every execution would
//!   grow without bound and cost rent forever. The per-settlement detail lives
//!   in events, which are cheap and are what an indexer should read.
//! - **Storage is rented**, so an agent's record extends its own TTL on every
//!   update. A reputation entry that expires takes the agent's history with it.

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, Address, Env, String, Vec,
};

/// Ledgers per day at Stellar's roughly 5 second close time.
const LEDGERS_PER_DAY: u32 = 17_280;

/// Reputation is long-lived by design — it is the asset an agent accumulates.
const TTL_EXTENSION_LEDGERS: u32 = LEDGERS_PER_DAY * 180;
const TTL_THRESHOLD_LEDGERS: u32 = LEDGERS_PER_DAY * 90;

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct Agent {
    pub address: Address,
    /// Free-form strategy label, e.g. "twap". Not interpreted here.
    pub strategy: String,
    /// False stops an agent competing without erasing its history.
    pub active: bool,
    pub registered_at: u64,
}

/// Measured performance. Every field is written by settlement, never by the
/// agent.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct Reputation {
    pub executions: u64,
    /// Executions delivered within the intent's slippage tolerance.
    pub wins: u64,
    /// Sum of realised slippage in basis points, kept so the mean can be
    /// derived without storing a running average that would drift with
    /// repeated rounding.
    pub total_slippage_bps: u64,
    /// Largest single realised slippage, in basis points. A mean alone hides
    /// an agent that is usually fine and occasionally catastrophic.
    pub worst_slippage_bps: u32,
    pub last_settled_at: u64,
}

#[contracttype]
pub enum DataKey {
    /// The settlement contract, the only permitted writer of reputation.
    Settlement,
    Agent(Address),
    Reputation(Address),
    /// Every registered agent, so the set can be listed without an indexer.
    Roster,
}

#[contracterror]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialised = 1,
    NotInitialised = 2,
    AgentAlreadyRegistered = 3,
    AgentNotFound = 4,
    AgentInactive = 5,
}

/// Emitted when an agent joins the roster.
#[contractevent]
#[derive(Clone, Debug, PartialEq)]
pub struct Registered {
    #[topic]
    pub agent: Address,
    pub strategy: String,
}

/// Emitted on every recorded settlement.
///
/// Carries the per-execution detail that is deliberately not kept in storage:
/// events are cheap and permanent, while a growing on-chain log would cost rent
/// forever. An indexer rebuilds full history from these.
#[contractevent]
#[derive(Clone, Debug, PartialEq)]
pub struct SettlementRecorded {
    #[topic]
    pub agent: Address,
    pub slippage_bps: u32,
    pub within_tolerance: bool,
}

#[contract]
pub struct AgentRegistry;

#[contractimpl]
impl AgentRegistry {
    /// Bind the registry to the settlement contract allowed to record outcomes.
    ///
    /// Callable once. If this could be rotated, whoever could rotate it could
    /// point it at an address that writes whatever reputation it likes.
    pub fn initialise(env: Env, settlement: Address) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Settlement) {
            return Err(Error::AlreadyInitialised);
        }
        env.storage()
            .instance()
            .set(&DataKey::Settlement, &settlement);
        Ok(())
    }

    /// Register an agent.
    ///
    /// Self-service: the agent authorises its own registration. Registering is
    /// not a privilege — it grants no reputation, and an agent still has to win
    /// competitions and have settlement record the result.
    pub fn register(env: Env, agent: Address, strategy: String) -> Result<(), Error> {
        agent.require_auth();

        let key = DataKey::Agent(agent.clone());
        if env.storage().persistent().has(&key) {
            return Err(Error::AgentAlreadyRegistered);
        }

        let record = Agent {
            address: agent.clone(),
            strategy,
            active: true,
            registered_at: env.ledger().timestamp(),
        };
        env.storage().persistent().set(&key, &record);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD_LEDGERS, TTL_EXTENSION_LEDGERS);

        // Starts at zero rather than being created lazily on first settlement,
        // so a newly registered agent is visible to anything reading the
        // leaderboard instead of missing until it first wins.
        let reputation = Reputation {
            executions: 0,
            wins: 0,
            total_slippage_bps: 0,
            worst_slippage_bps: 0,
            last_settled_at: 0,
        };
        let rep_key = DataKey::Reputation(agent.clone());
        env.storage().persistent().set(&rep_key, &reputation);
        env.storage()
            .persistent()
            .extend_ttl(&rep_key, TTL_THRESHOLD_LEDGERS, TTL_EXTENSION_LEDGERS);

        let mut roster: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::Roster)
            .unwrap_or_else(|| Vec::new(&env));
        roster.push_back(agent.clone());
        env.storage().persistent().set(&DataKey::Roster, &roster);
        env.storage().persistent().extend_ttl(
            &DataKey::Roster,
            TTL_THRESHOLD_LEDGERS,
            TTL_EXTENSION_LEDGERS,
        );

        Registered {
            agent,
            strategy: record.strategy,
        }
        .publish(&env);

        Ok(())
    }

    /// Stop or resume an agent competing.
    ///
    /// Deactivation rather than removal: proposals and settlements reference an
    /// agent, and deleting the record would orphan history that reputation is
    /// built from.
    pub fn set_active(env: Env, agent: Address, active: bool) -> Result<(), Error> {
        agent.require_auth();

        let key = DataKey::Agent(agent.clone());
        let mut record: Agent = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::AgentNotFound)?;

        record.active = active;
        env.storage().persistent().set(&key, &record);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD_LEDGERS, TTL_EXTENSION_LEDGERS);

        Ok(())
    }

    /// Record a settled execution against an agent.
    ///
    /// Callable only by settlement. This is the single write path for
    /// reputation, and it is what makes the numbers mean anything.
    pub fn record_settlement(
        env: Env,
        agent: Address,
        slippage_bps: u32,
        within_tolerance: bool,
    ) -> Result<(), Error> {
        let settlement: Address = env
            .storage()
            .instance()
            .get(&DataKey::Settlement)
            .ok_or(Error::NotInitialised)?;
        settlement.require_auth();

        let key = DataKey::Reputation(agent.clone());
        let mut reputation: Reputation = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::AgentNotFound)?;

        reputation.executions += 1;
        if within_tolerance {
            reputation.wins += 1;
        }
        reputation.total_slippage_bps += slippage_bps as u64;
        if slippage_bps > reputation.worst_slippage_bps {
            reputation.worst_slippage_bps = slippage_bps;
        }
        reputation.last_settled_at = env.ledger().timestamp();

        env.storage().persistent().set(&key, &reputation);
        env.storage()
            .persistent()
            .extend_ttl(&key, TTL_THRESHOLD_LEDGERS, TTL_EXTENSION_LEDGERS);

        SettlementRecorded {
            agent,
            slippage_bps,
            within_tolerance,
        }
        .publish(&env);

        Ok(())
    }

    pub fn get_agent(env: Env, agent: Address) -> Result<Agent, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Agent(agent))
            .ok_or(Error::AgentNotFound)
    }

    pub fn get_reputation(env: Env, agent: Address) -> Result<Reputation, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Reputation(agent))
            .ok_or(Error::AgentNotFound)
    }

    /// Mean realised slippage in basis points, or zero for an agent that has
    /// never settled.
    ///
    /// Derived on read from the running total rather than stored, so repeated
    /// rounding cannot accumulate into a figure that drifts from the sum it
    /// claims to summarise.
    pub fn average_slippage_bps(env: Env, agent: Address) -> Result<u32, Error> {
        let reputation: Reputation = env
            .storage()
            .persistent()
            .get(&DataKey::Reputation(agent))
            .ok_or(Error::AgentNotFound)?;

        if reputation.executions == 0 {
            return Ok(0);
        }
        Ok((reputation.total_slippage_bps / reputation.executions) as u32)
    }

    pub fn roster(env: Env) -> Vec<Address> {
        env.storage()
            .persistent()
            .get(&DataKey::Roster)
            .unwrap_or_else(|| Vec::new(&env))
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
