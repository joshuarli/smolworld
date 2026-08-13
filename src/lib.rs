//! `smolworld` is a deliberately small, macOS-local orchestration proof of
//! concept. It owns an isolated Ethernet segment and delegates every VM
//! lifecycle operation to the companion `smolvm` CLI.

mod cli;
mod config;
mod gateway;
mod model;
mod runtime;
mod smolvm;
mod state;
mod switch;

pub(crate) type Result<T> = std::result::Result<T, String>;

/// Parse the command line and run the selected local-world operation.
pub fn run() -> Result<()> {
    runtime::run(cli::parse_cli(std::env::args().skip(1).collect())?)
}
