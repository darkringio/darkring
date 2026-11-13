#!/usr/bin/env bash
# Start the devnet desk service on 127.0.0.1:5374 (DESK_PORT to change it).
# It holds the maker wallets from keys/ and proves their transactions on this
# machine; traders prove their own in the browser. The web app reaches it
# through its /api proxy.
#
# Needs .env with HELIUS_API_KEY, the wallets scripts/devnet-setup.sh makes,
# and the Zolana tools scripts/setup-zk-tools.sh and build-localnet.sh fetch.
# The key is stripped from everything the service prints.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
set -a
# shellcheck disable=SC1091
source "$root/.env"
# ZOLANA_CLI_BIN and PROVER_BIN, for the local prover.
# shellcheck disable=SC1091
source "$root/target/localnet.env"
set +a

binary="$root/target/release/perps-desk"
if [[ ! -x $binary ]]; then
    PATH="$HOME/.cargo/bin:$PATH" cargo build --release --locked -p perps-desk \
        --manifest-path "$root/Cargo.toml"
fi
cd "$root"
"$binary" 2>&1 | sed -u "s/$HELIUS_API_KEY/REDACTED/g"
