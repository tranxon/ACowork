//! rollball-gateway CLI entry point

use clap::Parser;
use rollball_gateway::cli::Cli;

fn main() {
    let cli = Cli::parse();
    if let Err(e) = cli.run() {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
