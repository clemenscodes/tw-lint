use anyhow::Result;
use clap::Parser;
use tw_lint::cli::{CliArgs, LintConfig};
use tw_lint::{join, report, session};

fn main() -> Result<()> {
    let args = CliArgs::parse();
    let config = LintConfig::resolve(args)?;

    // A configured container joins classes per block and streams via chunked
    // synthetic documents.
    if config.uses_container() {
        if config.fix {
            join::run_join_fix(&config)?;
        } else if join::run_join_check(&config)? > 0 {
            std::process::exit(1);
        }
        return Ok(());
    }

    if config.fix {
        session::run_fix(&config)?;
        return Ok(());
    }

    let results = session::run_session(&config)?;
    print!("{}", report::render(&results));
    if report::fatal_count(&results) > 0 {
        std::process::exit(1);
    }
    Ok(())
}
