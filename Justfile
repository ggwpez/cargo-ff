big_repo := env_var_or_default("FFMT_BIG_REPO", "/Users/vados/Documents/work/sdk2")

default:
    @just --list

build:
    cargo build --release

install:
    cargo install --path . --force

test:
    cargo test

# Equivalence + producer tests against a real workspace (default: sdk2).
test-big:
    FFMT_BIG_REPO={{big_repo}} cargo test --release -- --ignored --nocapture

# Compare wall time of cargo +nightly fmt vs cargo +nightly ffmt on big_repo.
bench: install
    cd {{big_repo}} && time cargo +nightly fmt --check >/dev/null 2>&1 || true
    cd {{big_repo}} && time cargo +nightly ffmt --check >/dev/null 2>&1 || true

check:
    cargo clippy --all-targets -- -D warnings
    cargo fmt --check
