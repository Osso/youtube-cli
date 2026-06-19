#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

cargo install --force --path .

echo "Installed youtube to ~/.cargo/bin"
