#!/usr/bin/env bash
# Deploy (or upgrade) the perps program on devnet at its declared address.
#
# The program verifies proofs against insecure test keys, so devnet is as far
# as this build may go. It calls the Rings shielded pool that is already on
# devnet (ring1GaLCuJnkUMVkX2JeZLTTX17wKsV4fZmPVzXANW, the same v1.0.0 binary
# the localnet tests load) and reads Pyth price update accounts.
#
# Uses the project's own Solana CLI in .tools/agave, never the machine's
# global install. The deployer pays for the program account and becomes its
# upgrade authority:
#
#   DEPLOYER_KEYPAIR   default keys/devnet-deployer.json
#   DEVNET_RPC_URL     default https://api.devnet.solana.com
#
# Builds the devnet binary into target/deploy-devnet, apart from the one the
# localnet tests load: it is the same program with the `devnet-oracle`
# feature, which accepts an oracle price for ten minutes instead of one.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
solana="$root/.tools/agave/solana-release/bin/solana"
program_so="$root/target/deploy-devnet/perps_program.so"
program_keypair="$root/keys/perps_program-keypair.json"
deployer="${DEPLOYER_KEYPAIR:-$root/keys/devnet-deployer.json}"
rpc_url="${DEVNET_RPC_URL:-https://api.devnet.solana.com}"

PATH="$root/.tools/agave/solana-release/bin:$HOME/.cargo/bin:$PATH" \
    cargo build-sbf --manifest-path "$root/program/Cargo.toml" --tools-version v1.54 \
    --sbf-out-dir "$root/target/deploy-devnet" \
    -- --locked --features bpf-entrypoint,devnet-oracle

for file in "$solana" "$program_so" "$program_keypair" "$deployer"; do
    if [[ ! -f $file ]]; then
        echo "missing $file" >&2
        exit 1
    fi
done

# Never deploy a binary whose keys are not the checked ones.
"$root/scripts/check-keys.sh" >/dev/null

genesis=$("$solana" genesis-hash --url "$rpc_url")
if [[ $genesis != EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG ]]; then
    echo "$rpc_url is not devnet (genesis $genesis)" >&2
    exit 1
fi

program_id=$("$solana" address -k "$program_keypair")
authority=$("$solana" address -k "$deployer")
balance=$("$solana" balance "$authority" --url "$rpc_url" --lamports | awk '{print $1}')
# The deploy holds the program's rent twice for a moment: once in the upload
# buffer and once in the program account.
size=$(wc -c <"$program_so")
rent=$("$solana" rent "$size" --url "$rpc_url" --lamports | awk '/Rent-exempt minimum/ {print $3}')
needed=$((rent * 2 + 20000000))

echo "Program:   $program_id"
echo "Authority: $authority"
echo "Balance:   $balance lamports, deploy needs about $needed"
if ((balance < needed)); then
    echo "Send devnet SOL to $authority and run this again." >&2
    exit 1
fi

"$solana" program deploy "$program_so" \
    --program-id "$program_keypair" \
    --keypair "$deployer" \
    --upgrade-authority "$deployer" \
    --url "$rpc_url"

"$solana" program show "$program_id" --url "$rpc_url"
echo "https://explorer.solana.com/address/$program_id?cluster=devnet"
