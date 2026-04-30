# Cargo Fast Format (`cargo-ff`)

Drop-in replacement for `cargo fmt`. Same flags, byte-identical file output, ~5.5× faster on large
workspaces by parallelizing across crates.

## Install

```sh
cargo install --locked cargo-ff
```

## Usage

Same flags as `cargo fmt`:

```sh
cargo +nightly ff              # format the current package
cargo +nightly ff --all        # format every workspace member
cargo +nightly ff --check      # check only, exit non-zero on diff
cargo +nightly ff -p mycrate   # format specific package(s)
```

## Pre-commit hook

To make `git commit` fail on unformatted files:

```sh
[ -f .git/hooks/pre-commit ] || printf '#!/usr/bin/env sh\n' > .git/hooks/pre-commit
echo 'cargo +nightly ff --check --all || exit' >> .git/hooks/pre-commit
chmod +x .git/hooks/pre-commit
```

## Experimental mtime cache

`--ff-experimental-cache` records each crate's `*.rs` mtimes after a successful run. On the next run,
crates whose fingerprint is unchanged are not dispatched to rustfmt at all. ~5× speedup on a clean
tree.

```sh
cargo +nightly ff --all --ff-experimental-cache
```

Cache lives at `~/.cache/cargo-ff/fingerprints.tsv`, salted by the `rustfmt --version` output and
the workspace-root `.rustfmt.toml`. A mismatch on either invalidates everything.

Soundness caveat: cross-crate `#[path = "../other/foo.rs"]` references aren't tracked. If the
external file changes, we may falsely skip.
