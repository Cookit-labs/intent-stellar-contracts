#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, AuthorizedFunction, AuthorizedInvocation, Ledger},
    Env, IntoVal, Symbol,
};

struct Fixture {
    env: Env,
    registry: AgentRegistryClient<'static>,
    settlement: Address,
    agent: Address,
}

fn setup() -> Fixture {
    let env = Env::default();
    env.mock_all_auths();

    let settlement = Address::generate(&env);
    let agent = Address::generate(&env);

    let id = env.register(AgentRegistry, ());
    let registry = AgentRegistryClient::new(&env, &id);
    registry.initialise(&settlement);

    Fixture {
        env,
        registry,
        settlement,
        agent,
    }
}

impl Fixture {
    fn strategy(&self, s: &str) -> String {
        String::from_str(&self.env, s)
    }
}

#[test]
fn registers_an_agent_with_zeroed_reputation() {
    let f = setup();
    f.registry.register(&f.agent, &f.strategy("twap"));

    let agent = f.registry.get_agent(&f.agent);
    assert_eq!(agent.address, f.agent);
    assert!(agent.active);

    // Created eagerly so a new agent is visible to the leaderboard rather than
    // missing until its first win.
    let rep = f.registry.get_reputation(&f.agent);
    assert_eq!(rep.executions, 0);
    assert_eq!(rep.wins, 0);
}

#[test]
fn refuses_to_register_the_same_agent_twice() {
    let f = setup();
    f.registry.register(&f.agent, &f.strategy("twap"));

    assert_eq!(
        f.registry.try_register(&f.agent, &f.strategy("momentum")),
        Err(Ok(Error::AgentAlreadyRegistered))
    );
}

#[test]
fn records_a_settlement_and_accumulates_counters() {
    let f = setup();
    f.registry.register(&f.agent, &f.strategy("twap"));

    f.registry.record_settlement(&f.agent, &50, &true);
    f.registry.record_settlement(&f.agent, &150, &false);

    let rep = f.registry.get_reputation(&f.agent);
    assert_eq!(rep.executions, 2);
    assert_eq!(rep.wins, 1);
    assert_eq!(rep.total_slippage_bps, 200);
    assert_eq!(rep.worst_slippage_bps, 150);
}

/// The property the whole contract exists to guarantee. If an agent could write
/// its own reputation, the leaderboard would be self-reported and the claim to
/// persistent reputation would be empty.
#[test]
fn an_agent_cannot_write_its_own_reputation() {
    let env = Env::default();

    let settlement = Address::generate(&env);
    let agent = Address::generate(&env);

    let id = env.register(AgentRegistry, ());
    let registry = AgentRegistryClient::new(&env, &id);

    env.mock_all_auths();
    registry.initialise(&settlement);
    registry.register(&agent, &String::from_str(&env, "twap"));

    // From here, only the agent's own authority is granted — settlement's is
    // not. The call must fail rather than fall through to the write.
    env.set_auths(&[]);
    env.mock_auths(&[soroban_sdk::testutils::MockAuth {
        address: &agent,
        invoke: &soroban_sdk::testutils::MockAuthInvoke {
            contract: &id,
            fn_name: "record_settlement",
            args: (agent.clone(), 0_u32, true).into_val(&env),
            sub_invokes: &[],
        },
    }]);

    let result = registry.try_record_settlement(&agent, &0, &true);
    assert!(
        result.is_err(),
        "an agent must not be able to record its own settlement"
    );
}

#[test]
fn settlement_authority_is_required_and_recorded() {
    let f = setup();
    f.registry.register(&f.agent, &f.strategy("twap"));
    f.registry.record_settlement(&f.agent, &25, &true);

    // The recorded authorisation should name the settlement contract, not the
    // agent whose reputation changed.
    let auths = f.env.auths();
    let authorised_by_settlement = auths.iter().any(|(addr, invocation)| {
        *addr == f.settlement
            && matches!(
                invocation,
                AuthorizedInvocation {
                    function: AuthorizedFunction::Contract((_, name, _)),
                    ..
                } if name == &Symbol::new(&f.env, "record_settlement")
            )
    });
    assert!(
        authorised_by_settlement,
        "record_settlement should require the settlement contract's authority"
    );
}

#[test]
fn averages_slippage_across_executions() {
    let f = setup();
    f.registry.register(&f.agent, &f.strategy("twap"));

    f.registry.record_settlement(&f.agent, &100, &true);
    f.registry.record_settlement(&f.agent, &200, &true);

    assert_eq!(f.registry.average_slippage_bps(&f.agent), 150);
}

#[test]
fn reports_zero_average_for_an_agent_that_has_never_settled() {
    let f = setup();
    f.registry.register(&f.agent, &f.strategy("twap"));

    // Guards the divide-by-zero path.
    assert_eq!(f.registry.average_slippage_bps(&f.agent), 0);
}

#[test]
fn keeps_the_worst_case_not_just_the_mean() {
    let f = setup();
    f.registry.register(&f.agent, &f.strategy("twap"));

    // An agent that is usually fine and occasionally terrible should not be
    // able to hide behind an average.
    for bps in [10_u32, 10, 10, 900] {
        f.registry.record_settlement(&f.agent, &bps, &true);
    }

    let rep = f.registry.get_reputation(&f.agent);
    assert_eq!(rep.worst_slippage_bps, 900);
    assert_eq!(f.registry.average_slippage_bps(&f.agent), 232);
}

#[test]
fn deactivating_preserves_history() {
    let f = setup();
    f.registry.register(&f.agent, &f.strategy("twap"));
    f.registry.record_settlement(&f.agent, &50, &true);

    f.registry.set_active(&f.agent, &false);

    assert!(!f.registry.get_agent(&f.agent).active);
    assert_eq!(
        f.registry.get_reputation(&f.agent).executions,
        1,
        "deactivation must not erase what an agent has done"
    );
}

#[test]
fn can_be_reactivated() {
    let f = setup();
    f.registry.register(&f.agent, &f.strategy("twap"));

    f.registry.set_active(&f.agent, &false);
    f.registry.set_active(&f.agent, &true);

    assert!(f.registry.get_agent(&f.agent).active);
}

#[test]
fn lists_every_registered_agent() {
    let f = setup();
    let second = Address::generate(&f.env);

    f.registry.register(&f.agent, &f.strategy("twap"));
    f.registry.register(&second, &f.strategy("momentum"));

    let roster = f.registry.roster();
    assert_eq!(roster.len(), 2);
    assert!(roster.contains(&f.agent));
    assert!(roster.contains(&second));
}

#[test]
fn reports_an_unknown_agent_rather_than_panicking() {
    let f = setup();
    let stranger = Address::generate(&f.env);

    assert_eq!(
        f.registry.try_get_agent(&stranger),
        Err(Ok(Error::AgentNotFound))
    );
    assert_eq!(
        f.registry.try_record_settlement(&stranger, &10, &true),
        Err(Ok(Error::AgentNotFound))
    );
}

#[test]
fn cannot_be_initialised_twice() {
    let f = setup();
    let other = Address::generate(&f.env);

    assert_eq!(
        f.registry.try_initialise(&other),
        Err(Ok(Error::AlreadyInitialised))
    );
}

#[test]
fn timestamps_the_last_settlement() {
    let f = setup();
    f.registry.register(&f.agent, &f.strategy("twap"));

    f.env.ledger().with_mut(|l| l.timestamp = 1_700_000_000);
    f.registry.record_settlement(&f.agent, &10, &true);

    assert_eq!(
        f.registry.get_reputation(&f.agent).last_settled_at,
        1_700_000_000
    );
}
