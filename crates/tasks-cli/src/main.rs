//! `loadtable`: thin wrapper over [`tasks_cli::real_main`].

use std::process::ExitCode;

fn main() -> ExitCode {
    let code = tasks_cli::real_main();
    ExitCode::from(u8::try_from(code & 0xff).unwrap_or(1))
}
