#!/usr/bin/env bash
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")"

cargo run --quiet --manifest-path ../../argent/Cargo.toml -- build ag/app.ag --out build/argent
git diff --exit-code -- build/argent
cargo fmt --check
cargo check --workspace --all-targets
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
git diff --check HEAD -- .
