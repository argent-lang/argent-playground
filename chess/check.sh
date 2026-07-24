#!/usr/bin/env bash
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")"

allow_generated_diff=false

case "${1:-}" in
    "")
        ;;
    --regen)
        allow_generated_diff=true
        ;;
    -h|--help)
        cat <<'USAGE'
Usage: ./check.sh [--regen]

Regenerates and verifies the Chess application.

Options:
  --regen     Regenerate tracked build output without requiring a clean diff.
  -h, --help  Show this help.
USAGE
        exit 0
        ;;
    *)
        echo "unknown argument: $1" >&2
        echo "try: ./check.sh --help" >&2
        exit 2
        ;;
esac

cargo run --quiet --manifest-path ../../argent/Cargo.toml -- build ag/app.ag --out build
if [[ "$allow_generated_diff" = false ]]; then
    git diff --exit-code -- build
fi

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
