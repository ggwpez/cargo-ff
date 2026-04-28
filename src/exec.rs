use crate::types::{Config, CrateResult, CrateUnit};
use crossbeam_channel::{Receiver, Sender};
use std::process::Command;

pub(crate) fn worker(rx: Receiver<CrateUnit>, tx: Sender<CrateResult>, cfg: &Config) {
    while let Ok(unit) = rx.recv() {
        let result = format_unit(&unit, cfg);
        if tx.send(result).is_err() {
            break;
        }
    }
}

fn format_unit(unit: &CrateUnit, cfg: &Config) -> CrateResult {
    let mut cmd = Command::new("rustfmt");
    cmd.current_dir(&unit.manifest_dir);
    cmd.arg("--edition").arg(unit.edition.as_str());
    if cfg.check {
        cmd.arg("--check");
    }
    for arg in &cfg.rustfmt_args {
        cmd.arg(arg);
    }
    for f in &unit.files {
        cmd.arg(f);
    }

    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            let msg = format!(
                "failed to spawn rustfmt for crate {}: {e}\n",
                unit.manifest_dir.display()
            );
            return CrateResult {
                sort_key: unit.manifest_dir.clone(),
                stdout: Vec::new(),
                stderr: msg.into_bytes(),
                exit_code: 2,
                files: unit.files.clone(),
            };
        }
    };

    CrateResult {
        sort_key: unit.manifest_dir.clone(),
        stdout: output.stdout,
        stderr: output.stderr,
        exit_code: output.status.code().unwrap_or(-1),
        files: unit.files.clone(),
    }
}
