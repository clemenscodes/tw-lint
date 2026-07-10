use anyhow::Result;
use clap::Parser;
use tw_lint::cli::{CliArgs, LintConfig};
use tw_lint::{join, report, session};

fn main() -> Result<()> {
    let args = CliArgs::parse();
    let config = LintConfig::resolve(args)?;

    if config.fix {
        if config.uses_container() {
            join::run_join_fix(&config)?;
        } else {
            session::run_fix(&config)?;
        }
        return Ok(());
    }

    let results = if config.uses_container() {
        join::run_join_check(&config)?
    } else {
        session::run_session(&config)?
    };
    print!("{}", report::render(&results));

    if report::fatal_count(&results) > 0 {
        std::process::exit(1);
    }
    Ok(())
}
