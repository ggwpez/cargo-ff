use cargo_ff::{Config, Error};

#[test]
fn run_rejects_zero_workers() {
    let cfg = Config {
        workers: Some(0),
        ..Config::default()
    };
    let err = cargo_ff::run(&cfg).expect_err("workers=0 must be rejected");
    assert!(matches!(err, Error::InvalidWorkers(0)));
}

#[cfg(feature = "cli")]
#[test]
fn cli_rejects_zero_workers() {
    use clap::Parser;

    let err = cargo_ff::cli::Cli::try_parse_from(["cargo-ff", "--workers", "0"])
        .expect_err("clap must reject workers=0");
    assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
}
