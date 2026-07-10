use anyhow::Result;
use clap::Parser;
use tw_lint::cli::{CliArgs, LintConfig};
use tw_lint::{report, session};

fn main() -> Result<()> {
    let args = CliArgs::parse();
    let config = LintConfig::resolve(args)?;

    let results = session::run_session(&config)?;
    print!("{}", report::render(&results));

    if !config.fix && report::fatal_count(&results) > 0 {
        std::process::exit(1);
    }
    Ok(())
}
