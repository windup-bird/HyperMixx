#!/usr/bin/env bash
# Everything CI and a pre-commit check should agree on. Run from the repository root.
#
#   ./scripts/check.sh
#
# Note: `cargo fmt --check` is deliberately *not* part of this. The tree predates rustfmt (its
# import ordering in particular), so enforcing it would reformat most of the workspace in one
# unrelated diff. Run `cargo fmt --all` by hand if you want that.
set -euo pipefail

cd "$(dirname "$0")/.."

# `-D warnings` so a clippy lint is a failure here rather than a note nobody reads.
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
