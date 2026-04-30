big_repo := env_var_or_default("FF_BIG_REPO", "/Users/vados/Documents/work/sdk2")

default:
    @just --list

c: check
check:
    cargo clippy --all-targets -- -D warnings
    cargo fmt --check

b: build
build:
    cargo build --release

i: install
install:
    cargo install --path . --force

t: test
test:
    cargo test

# Equivalence + producer tests against a real workspace (default: sdk2).
test-big:
    FF_BIG_REPO={{big_repo}} cargo test --release -- --ignored --nocapture

# Compare wall time of cargo +nightly fmt vs cargo +nightly ff on big_repo.
bench: install
    cd {{big_repo}} && time cargo +nightly fmt --check >/dev/null 2>&1 || true
    cd {{big_repo}} && time cargo +nightly ff --check >/dev/null 2>&1 || true

# 5-run wall-time bench at varying worker counts.
bench-workers:
    cargo build --profile profiling
    @echo "workers,run,seconds" > /tmp/ff-bench.csv
    @for w in 1 2 4 8 12 16 24 32; do \
      for i in 1 2 3; do \
        s=$$(python3 -c 'import time; print(time.time())'); \
        ./target/profiling/cargo-ff ff --check --all --workers $$w --manifest-path {{big_repo}}/Cargo.toml >/dev/null 2>&1; \
        e=$$(python3 -c 'import time; print(time.time())'); \
        d=$$(python3 -c "print($$e - $$s)"); \
        echo "  w=$$w run=$$i  $${d}s"; \
        echo "$$w,$$i,$$d" >> /tmp/ff-bench.csv; \
      done; \
    done
    @echo "wrote /tmp/ff-bench.csv"

# Sampling profile of one full run; opens samply UI in the browser.
# Captures every spawned rustfmt as well, so the flamegraph reflects
# both our orchestration and rustfmt's per-invocation cost.
flamegraph:
    cargo build --profile profiling
    RUSTUP_TOOLCHAIN=nightly samply record -- ./target/profiling/cargo-ff ff --check --all --manifest-path {{big_repo}}/Cargo.toml

# Profile a single rustfmt invocation to see the per-invocation overhead floor.
flamegraph-rustfmt:
    RUSTUP_TOOLCHAIN=nightly samply record -- rustfmt --edition 2024 --check src/*.rs

# Save a profile to /tmp without opening the UI (handy for CI / sharing).
flamegraph-save:
    cargo build --profile profiling
    RUSTUP_TOOLCHAIN=nightly samply record --save-only --no-open -o /tmp/ff-profile.json.gz -- ./target/profiling/cargo-ff ff --check --all --manifest-path {{big_repo}}/Cargo.toml
    @echo "load with: samply load /tmp/ff-profile.json.gz"
