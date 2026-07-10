use anyhow::Result;
use clap::Parser;
use tw_lint::cli::{CliArgs, LintConfig};

fn main() -> Result<()> {
    let args = CliArgs::parse();
    let config = LintConfig::resolve(args)?;
    eprintln!("resolved config for root {}", config.root.display());
    Ok(())
}
