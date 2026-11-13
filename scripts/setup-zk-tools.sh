#!/usr/bin/env bash
# Install the Zolana release CLI that matches the pinned SDK crates and print
# ZOLANA_CLI_BIN. The CLI bundles Xark; `zk compile` and check-keys.sh verify
# its output. --check-pins only checks the Zolana Git pins in Cargo.toml.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
# shellcheck source=../test-utils/zolana-release.sh
source "$root/test-utils/zolana-release.sh"

check_pins "$root/Cargo.toml"
if [[ ${1:-} == --check-pins ]]; then
    exit 0
fi

install_zolana_cli "$root/.tools/$zolana_release" "$root/.tools/bin/zolana"
echo "ZOLANA_CLI_BIN=$root/.tools/bin/zolana"
