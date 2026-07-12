#!/usr/bin/env bash
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")"

cargo fmt --check
cargo check --all-targets
cargo test --all-targets
cargo clippy --all-targets -- -D warnings

for source in src/bin/*.rs; do
    bin="$(basename -- "$source" .rs)"
    echo "running $bin"
    cargo run --quiet --bin "$bin"
done

git diff --check
