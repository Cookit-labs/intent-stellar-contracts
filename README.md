# intent-stellar-contracts

Soroban escrow and settlement for [Intent](https://github.com/Cookit-labs/Intent)
on Stellar.

The Stellar counterpart to
[intent-core-contracts](https://github.com/Cookit-labs/intent-core-contracts),
which targets Arc. Same security model, different platform.

**Status:** escrow, settlement, and the agent registry are implemented, tested,
and deployed to Stellar testnet.

## Why this exists

Every product document states that agents never custody user funds. These
contracts are what makes that true rather than asserted: user capital sits in
escrow, agents receive permission to execute within stated bounds, and a
violating execution reverts.

Intent runs on multiple chains, with one backend coordinating them. The chain
layer is what differs; the auction, scoring, and reputation are shared.

## Deployed contracts

Stellar testnet. Machine-readable copy in [`deployments/testnet.json`](deployments/testnet.json).

| Contract | Id |
|----------|-----|
| Escrow | `CBQSLZJWLIWTDVQ2KA2CTVBTHIPQS5XNSDZIIU7HG6MEYX2N6O6IXRH3` |
| Settlement | `CAEJRTJHXNTFG3JEW3H72BQZEIYX5EHEGZTXGX576V6U6KGPL4RG4FAR` |
| Registry | `CBSEAQ75OOJ6I6Q7JK2JSIQOUOCHAZEEJLN4EUZDEPK2DQA5EBDAWYCJ` |

Network passphrase: `Test SDF Network ; September 2015`

The deployment file records a **WASM hash per contract**. A contract id says
which address to call, not which code answers; with the hash, a deployment can
be verified against this source rather than trusted.

Redeploying:

```bash
./scripts/deploy.sh testnet deployer
```

**SDF resets testnet periodically**, and every id above becomes invalid when it
happens. Re-running the script is the recovery path — that is why deployment is
a script rather than a list of commands in a README. Each contract binds to its
counterparties once and cannot be repointed, so all three must be redeployed
together.

## The asset, and trustlines

The escrow is asset-agnostic: `deposit` takes a token contract address, so it
works with any Stellar Asset Contract. In practice that is **USDC**, and on
Stellar USDC is an *issued asset*, not a native balance.

That distinction has no Arc equivalent and is the single biggest semantic
difference between the two chains:

- **Holding USDC requires a trustline to the issuer.** An account without one
  cannot hold the asset at all, and its balance reads as zero — which at the
  contract level is indistinguishable from an account that simply has none.
- Circle's testnet issuer is
  `GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5` (home domain
  `centre.io`). Testnet carries many unrelated self-issued assets also coded
  `USDC`, so **pin the issuer** — a test against the wrong one proves nothing.
- XLM is native and needs no trustline. It pays fees and reserves.

The escrow checks the depositor's balance before attempting the transfer, so a
missing trustline surfaces as a typed `InsufficientBalance` rather than a panic
from inside the token contract. `can_deposit` exposes the same check as a
read-only call, so the dapp can tell a user *why* a deposit would fail and offer
to create the trustline — a one-transaction fix, but only if the user is told
rather than shown a bare rejection.

**What is not enforced on-chain:** issuer authorisation flags. If an issuer
enables `AUTH_REQUIRED` and has not authorised the escrow, a transfer fails
inside the token contract. The Soroban test harness offers no way to enable that
flag on a locally registered asset, so it cannot be covered by a test here and
is deliberately left to integration testing against a real network.

## Setup

```bash
git clone https://github.com/Cookit-labs/intent-stellar-contracts
cd intent-stellar-contracts
cargo test              # native tests, fast
stellar contract build  # compile to WASM
```

Requires Rust stable and the [Stellar CLI](https://developers.stellar.org/docs/tools/cli).
The `wasm32v1-none` target is pinned in `rust-toolchain.toml`.

## Testing

```bash
cargo test --workspace
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

23 tests currently pass.

**`cargo test` runs natively, not on-chain.** It exercises contract logic
through Soroban's test environment, which is faithful for logic but does not
prove the contract deploys. `stellar contract build` is the check that catches
a contract compiling for the host and not for the chain, and CI runs both.

## How Soroban differs from the Arc implementation

Worth reading before porting anything across, because these are platform
differences rather than design choices.

**No implicit caller.** Solidity has `msg.sender`; Soroban does not. Every
address that must consent to an operation calls `require_auth()` explicitly. A
missing `require_auth` is not a warning — it means anyone can name someone else
as depositor and move their tokens.

**No native value transfer.** There is no `msg.value` or `payable`. Tokens move
through a token client, and the contract must be authorised to pull them.

**Storage is rented and can be archived.** Entries have a time-to-live that
must be extended or the entry disappears. An escrow whose entry expired while
holding funds would strand them, so `deposit` extends TTL to 30 days — well
past any plausible refund deadline.

**Errors are enum variants, not reverts.** A `#[contracterror]` enum returns
typed errors rather than reverting with a reason string, which makes them
cheaper to match on from a client.

## Security model

Identical in shape to the Arc contracts:

- **Escrow holds funds** from deposit until settlement or expiry
- **Only the settlement contract may release**, bound once at initialisation and
  not rotatable — a rotatable settlement address would make whoever can rotate
  it the real custodian
- **Refund is permissionless after the deadline**, and always pays the recorded
  depositor rather than the caller, so opening it up cannot redirect money
- **Status is consumed before any transfer**, so a re-entrant call finds an
  intent that is no longer funded

Every guard is mutation-tested: removed or inverted, with the suite required to
fail. That shows the tests notice a missing guard. It does not show the guards
are the right set — these contracts are unaudited.

## Layout

```
contracts/escrow/     IntentEscrow — custody
contracts/settlement/ not yet written
```

## Related

- [intent-core-contracts](https://github.com/Cookit-labs/intent-core-contracts) — Arc contracts
- [Backend](https://github.com/Cookit-labs/Backend) — Go marketplace backend
- [Intent-Agent](https://github.com/Cookit-labs/Intent-Agent) — Python agent service
- [Intent](https://github.com/Cookit-labs/Intent) — frontend
