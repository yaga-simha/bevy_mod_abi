#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Binary entrypoint for `cargo-bam`.

use std::process::ExitCode;

fn main() -> ExitCode {
    cargo_bam::run_from_args(std::env::args_os())
}
