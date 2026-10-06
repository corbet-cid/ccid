use clap::Parser;
use std::process::ExitCode;

fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("source-revision") {
        println!("{}", ccid::SOURCE_REVISION);
        return ExitCode::SUCCESS;
    }
    match ccid::status::run(ccid::status::Options::parse()) {
        Ok(report) => {
            println!("{report}");
            if report["complete"] == true {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            }
        }
        Err(error) => {
            eprintln!("ccid-status: {error}");
            ExitCode::from(2)
        }
    }
}
