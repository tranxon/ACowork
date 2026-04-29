//! rollball-verify CLI

use clap::Parser;
use rollball_sign::verify::verify_package;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "rollball-verify")]
#[command(about = "Verify .agent package signatures")]
struct Cli {
    /// .agent package path to verify
    package: PathBuf,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    println!("Verifying package: {}", cli.package.display());
    println!();

    let result = verify_package(&cli.package)?;

    if result.valid {
        println!("✓ Signature is VALID");
        println!("  Signer:         {}", result.signer);
        println!("  Sections:       {}", result.sections_count);
        println!("  Fingerprint:    {}", result.certificate_fingerprint);
    } else {
        println!("✗ Signature is INVALID");
        std::process::exit(1);
    }

    Ok(())
}
