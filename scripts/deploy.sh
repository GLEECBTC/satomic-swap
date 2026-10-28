#!/usr/bin/env bash
#
# Deploys or upgrades satomic-swap.
#
#   ./scripts/deploy.sh devnet
#   ./scripts/deploy.sh mainnet-beta
#
# See docs/DEPLOYMENT.md for the runbook this belongs to, including the upgrade-authority policy
# and how to verify the deployed binary afterwards.

set -euo pipefail

CLUSTER="${1:-}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROGRAM_KEYPAIR="${PROGRAM_KEYPAIR:-$REPO_ROOT/target/deploy/satomic_swap-keypair.json}"
PROGRAM_SO="$REPO_ROOT/target/deploy/satomic_swap.so"

die() { echo "error: $*" >&2; exit 1; }

case "$CLUSTER" in
  devnet|testnet|mainnet-beta|localnet) ;;
  *) die "usage: $0 {devnet|testnet|mainnet-beta|localnet}" ;;
esac

command -v solana >/dev/null || die "the Solana CLI is not on PATH"
command -v anchor >/dev/null || die "anchor is not on PATH"

# The program keypair *is* the program address. It is gitignored, so a lost copy means this
# address can never be deployed to or upgraded again.
[ -f "$PROGRAM_KEYPAIR" ] || die "no program keypair at $PROGRAM_KEYPAIR (see docs/DEPLOYMENT.md)"

DECLARED=$(grep -oE 'declare_id!\("[^"]+"\)' "$REPO_ROOT/programs/satomic-swap/src/lib.rs" | grep -oE '[A-Za-z0-9]{32,}')
KEYPAIR_ID=$(solana-keygen pubkey "$PROGRAM_KEYPAIR")
[ "$DECLARED" = "$KEYPAIR_ID" ] || die "declare_id! is $DECLARED but the keypair is $KEYPAIR_ID"

echo "Building..."
(cd "$REPO_ROOT" && anchor build)
[ -f "$PROGRAM_SO" ] || die "no program binary at $PROGRAM_SO"

echo
echo "  cluster:    $CLUSTER"
echo "  program:    $KEYPAIR_ID"
echo "  binary:     $PROGRAM_SO ($(wc -c < "$PROGRAM_SO") bytes)"
echo "  authority:  $(solana address)"
echo "  payer:      $(solana address) ($(solana balance --url "$CLUSTER" 2>/dev/null || echo 'balance unknown'))"
echo

# A mainnet deploy spends real SOL and, once the upgrade authority is revoked, cannot be undone.
if [ "$CLUSTER" = "mainnet-beta" ]; then
  echo "This deploys to MAINNET. The program has to have been audited first (docs/DEPLOYMENT.md)."
  read -r -p "Type the program id to confirm: " CONFIRM
  [ "$CONFIRM" = "$KEYPAIR_ID" ] || die "confirmation did not match; nothing was deployed"
fi

solana program deploy \
  --url "$CLUSTER" \
  --program-id "$PROGRAM_KEYPAIR" \
  "$PROGRAM_SO"

echo
echo "Deployed. Verify what actually landed:"
echo "  solana program show --url $CLUSTER $KEYPAIR_ID"
echo
echo "Then follow docs/DEPLOYMENT.md to record the upgrade authority and publish a verified build."
