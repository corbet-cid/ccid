use ccid::pr_bridge::{run, Options};
use clap::Parser;
use std::process::ExitCode;

fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("source-revision") {
        println!("{}", ccid::SOURCE_REVISION);
        return ExitCode::SUCCESS;
    }
    match run(Options::parse()) {
        Ok(report) => {
            println!("{report}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("ccid-pr-bridge: {error}");
            ExitCode::from(2)
        }
    }
}
