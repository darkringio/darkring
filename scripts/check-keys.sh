#!/usr/bin/env bash
# Check generated Xark bytecode and keys against trusted checksums.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${PERPS_KEYS_DIR:-$root/target/zk/perps-program}"
shasum -a 256 --check --strict "$root/perps-keys.CHECKSUM"
