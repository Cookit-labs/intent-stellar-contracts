#!/usr/bin/env bash
#
# Deploy the Intent contracts to a Stellar network and record what was deployed.
#
#   ./scripts/deploy.sh testnet deployer
#
# Writes deployments/<network>.json with the contract ids and the WASM hash of
# each. The hash is the point: a contract id alone says which address to call,
# not which code is behind it. With the hash, anyone can verify a deployment
# matches this source instead of trusting that it does.
#
# Testnet is reset periodically by SDF, and every id here becomes invalid when
# that happens. Re-running this script is the recovery path, which is why it is
# a script rather than a sequence of commands in a README.

set -euo pipefail

NETWORK="${1:-testnet}"
SOURCE="${2:-deployer}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WASM_DIR="$ROOT/target/wasm32v1-none/release"
OUT_DIR="$ROOT/deployments"
OUT="$OUT_DIR/$NETWORK.json"

command -v stellar >/dev/null || {
  echo "stellar CLI not found. See https://developers.stellar.org/docs/tools/cli" >&2
  exit 1
}

echo "==> Building"
(cd "$ROOT" && stellar contract build >/dev/null)

for contract in intent_escrow intent_settlement intent_registry; do
  [ -f "$WASM_DIR/$contract.wasm" ] || {
    echo "missing $contract.wasm — build failed?" >&2
    exit 1
  }
done

deploy() {
  stellar contract deploy \
    --wasm "$WASM_DIR/$1.wasm" \
    --source "$SOURCE" \
    --network "$NETWORK" 2>/dev/null | tr -d '\r'
}

wasm_hash() {
  # Recorded so a deployment can be verified against this source rather than
  # trusted. Falls back to the local file hash if the CLI cannot report it.
  stellar contract info interface --wasm "$WASM_DIR/$1.wasm" >/dev/null 2>&1 || true
  sha256sum "$WASM_DIR/$1.wasm" | cut -d' ' -f1
}

echo "==> Deploying escrow"
ESCROW_ID="$(deploy intent_escrow)"
echo "    $ESCROW_ID"

echo "==> Deploying settlement"
SETTLEMENT_ID="$(deploy intent_settlement)"
echo "    $SETTLEMENT_ID"

echo "==> Deploying registry"
REGISTRY_ID="$(deploy intent_registry)"
echo "    $REGISTRY_ID"

# Each contract binds to its counterparties once and cannot be repointed, so
# ordering matters: nothing can be initialised until every id exists.
echo "==> Wiring escrow -> settlement"
stellar contract invoke --id "$ESCROW_ID" --source "$SOURCE" --network "$NETWORK" -- \
  initialise --settlement "$SETTLEMENT_ID" >/dev/null

echo "==> Wiring settlement -> escrow, validator"
VALIDATOR="$(stellar keys address "$SOURCE" | tr -d '\r')"
stellar contract invoke --id "$SETTLEMENT_ID" --source "$SOURCE" --network "$NETWORK" -- \
  initialise --escrow "$ESCROW_ID" --validator "$VALIDATOR" >/dev/null

echo "==> Wiring registry -> settlement"
stellar contract invoke --id "$REGISTRY_ID" --source "$SOURCE" --network "$NETWORK" -- \
  initialise --settlement "$SETTLEMENT_ID" >/dev/null

mkdir -p "$OUT_DIR"
cat > "$OUT" <<JSON
{
  "network": "$NETWORK",
  "networkPassphrase": "Test SDF Network ; September 2015",
  "horizonUrl": "https://horizon-testnet.stellar.org",
  "sorobanRpcUrl": "https://soroban-testnet.stellar.org",
  "deployedAt": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "validator": "$VALIDATOR",
  "contracts": {
    "escrow": {
      "id": "$ESCROW_ID",
      "wasmHash": "$(wasm_hash intent_escrow)"
    },
    "settlement": {
      "id": "$SETTLEMENT_ID",
      "wasmHash": "$(wasm_hash intent_settlement)"
    },
    "registry": {
      "id": "$REGISTRY_ID",
      "wasmHash": "$(wasm_hash intent_registry)"
    }
  }
}
JSON

echo "==> Wrote $OUT"
cat "$OUT"
