// Tests assert by panicking: unwrap/expect/panic are the idiomatic way to fail
// a test loudly, so the restriction lints that forbid them in library code do
// not apply here.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use cargo_ff::{Config, Error};

#[test]
fn run_rejects_zero_workers() {
    let mut cfg = Config::default();
    cfg.workers = Some(0);
    let err = cargo_ff::run(&cfg).expect_err("workers=0 must be rejected");
    assert!(matches!(err, Error::InvalidWorkers(0)));
}

#[test]
fn run_rejects_unknown_package() {
    // `-p <name>` for a package that isn't a workspace member must error rather
    // than silently format nothing. Runs `cargo metadata` on this crate, then
    // fails package selection before any rustfmt process is spawned.
    let mut cfg = Config::default();
    cfg.packages = vec!["definitely-not-a-real-package".to_string()];
    let err = cargo_ff::run(&cfg).expect_err("an unknown -p package must be rejected");
    match err {
        Error::UnknownPackages(pkgs) => {
            assert_eq!(pkgs, vec!["definitely-not-a-real-package".to_string()]);
        }
        other => panic!("expected UnknownPackages, got {other:?}"),
    }
}

#[cfg(feature = "cli")]
#[test]
fn cli_rejects_zero_workers() {
    use clap::Parser;

    let err = cargo_ff::cli::Cli::try_parse_from(["cargo-ff", "--ff-workers", "0"])
        .expect_err("clap must reject workers=0");
    assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
}
