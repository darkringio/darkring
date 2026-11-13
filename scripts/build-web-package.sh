#!/usr/bin/env bash
# Build the browser package of the program: the transaction builders, the
# state decoders and Xark's prover as WebAssembly, with the JavaScript that
# loads them. A trader's browser builds and proves its own opens and closes
# with it, so its keys never leave the tab.
#
# The package is built from the compiled circuits and keys already in
# target/zk/perps-program: nothing is compiled or generated again, and
# check-keys.sh confirms afterwards that they are still the pinned ones.
#
#   scripts/build-web-package.sh [out-dir]
#
# Writes to target/zk/perps-program/wasm unless told another directory, then
# prints the files a web app has to serve: the package, and for each circuit
# its circuit.xbc, pk.bin, vk.bin and sources.json.
#
# Needs the zolana CLI (scripts/setup-zk-tools.sh) and wasm-pack on PATH or
# in .tools/wasm-pack (cargo install wasm-pack --locked --root .tools/wasm-pack).
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
zolana_cli="${ZOLANA_CLI_BIN:-$root/.tools/bin/zolana}"
if [[ ! -x $zolana_cli ]]; then
    echo "No zolana CLI at $zolana_cli; run scripts/setup-zk-tools.sh first" >&2
    exit 1
fi
export PATH="$root/.tools/wasm-pack/bin:$HOME/.cargo/bin:$PATH"
if ! command -v wasm-pack >/dev/null; then
    echo "No wasm-pack; cargo install wasm-pack --locked --root .tools/wasm-pack" >&2
    exit 1
fi

artifacts="$root/target/zk/perps-program"
out="${1:-$artifacts/wasm}"

cd "$root"
"$zolana_cli" zk compile -p perps-program --skip-r1cs --skip-keys --wasm-out "$out" >&2
"$root/scripts/check-keys.sh" >&2

echo "package: $out"
for circuit in quote open settle cancel_quote; do
    echo "circuit $circuit: $artifacts/$circuit/{circuit.xbc,pk.bin,vk.bin,sources.json}"
done
