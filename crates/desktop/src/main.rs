//! `incu`: computer use for an agent, as an MCP server over stdio.
//!
//! The binary is only argument parsing and an exit code; everything it does is
//! in [`incord_desktop::cli`], which incode's `--desktop-mcp` wrapper shares.
//! Nothing is written to stdout except the protocol (and the text of `--help`,
//! `--version` and `stop`, none of which run beside a client).

use std::process::ExitCode;

fn main() -> ExitCode {
    let command = match incord_desktop::cli::parse(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(why) => {
            eprintln!("incu: {why}");
            return ExitCode::from(2);
        }
    };
    match incord_desktop::cli::run(command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("incu: {why}");
            ExitCode::FAILURE
        }
    }
}
