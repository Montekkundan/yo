#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features --document-private-items
cargo test --locked --all
cargo audit
cargo deny check
cargo build --release --locked
cargo package --locked --allow-dirty
