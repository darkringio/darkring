#!/usr/bin/env bash
# Prepare devnet for the desk service and for the end-to-end run in
# test/tests/devnet.rs. Safe to run again: every step is skipped once done.
#
#   1. A trader wallet and three maker wallets in keys/, each given SOL
#      for fees.
#   2. A 6-decimal test dollar whose mint authority is the deployer, with a
#      million of it in the deployer's token account. The test registers the
#      mint with the Rings pool; the service shields it to the wallets.
#
# Reads HELIUS_API_KEY from .env for the RPC, else uses the public endpoint.
# Uses the project's own Solana CLI in .tools/agave.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
bin="$root/.tools/agave/solana-release/bin"
keys="$root/keys"
deployer="$keys/devnet-deployer.json"

if [[ -f $root/.env ]]; then
    set -a
    # shellcheck disable=SC1091
    source "$root/.env"
    set +a
fi
if [[ -n ${HELIUS_API_KEY:-} ]]; then
    rpc_url="https://devnet.helius-rpc.com/?api-key=$HELIUS_API_KEY"
else
    rpc_url="https://api.devnet.solana.com"
fi

solana() { "$bin/solana" --url "$rpc_url" --keypair "$deployer" "$@"; }
token() { "$bin/spl-token" --url "$rpc_url" --fee-payer "$deployer" "$@"; }

# What each wallet needs for fees and its user-registry record.
wallet_lamports=300000000

for name in trader maker maker-2 maker-3; do
    file="$keys/devnet-$name.json"
    if [[ ! -f $file ]]; then
        "$bin/solana-keygen" new --no-bip39-passphrase --silent -o "$file" >/dev/null
        chmod 600 "$file"
    fi
    address=$("$bin/solana" address -k "$file")
    balance=$(solana balance "$address" --lamports | awk '{print $1}')
    if ((balance < wallet_lamports / 2)); then
        solana transfer "$address" 0.3 --allow-unfunded-recipient >/dev/null
    fi
    echo "$name: $address"
done

mint_keypair="$keys/devnet-mint.json"
if [[ ! -f $mint_keypair ]]; then
    "$bin/solana-keygen" new --no-bip39-passphrase --silent -o "$mint_keypair" >/dev/null
    chmod 600 "$mint_keypair"
fi
mint=$("$bin/solana" address -k "$mint_keypair")
owner=$("$bin/solana" address -k "$deployer")
# A fresh account can take a few seconds to show up on the RPC node.
held() {
    for _ in $(seq 15); do
        if token balance "$mint" --owner "$owner" 2>/dev/null; then
            return 0
        fi
        sleep 2
    done
    return 1
}

if ! solana account "$mint" >/dev/null 2>&1; then
    token create-token "$mint_keypair" --decimals 6 --mint-authority "$deployer" >/dev/null
fi
if ! token balance "$mint" --owner "$owner" >/dev/null 2>&1; then
    token create-account "$mint" --owner "$owner" >/dev/null 2>&1 || true
fi
supply=$(held)
if [[ ${supply%.*} -lt 1000 ]]; then
    token mint "$mint" 1000000 --recipient-owner "$owner" --mint-authority "$deployer" >/dev/null
    sleep 4
fi
echo "mint: $mint ($(held) held by the deployer)"
