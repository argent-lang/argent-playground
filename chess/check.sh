#!/usr/bin/env bash
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")"

cargo run --quiet --manifest-path ../../argent/Cargo.toml -- build ag/app.ag --out build
git diff --exit-code -- build

if [[ -e build/argent ]]; then
    echo "error: generated Chess output must be stored directly under build/" >&2
    exit 1
fi

if rg --quiet \
    'baseline/sil|silverscript_lang|silverscript-lang|compile_contract|load_contract_source' \
    chess-covenant Cargo.toml
then
    echo "error: Chess Rust code must not compile or load the handwritten SIL baseline" >&2
    exit 1
fi

cargo fmt --check
cargo check --workspace --all-targets
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
git diff --check HEAD -- .
