//! `stewardd`: the daemon on its own, for development and tests. Releases
//! ship it inside `steward` as `steward daemon`.

use clap::Parser;

#[derive(Parser)]
#[command(name = "stewardd", about = "The steward file index service", version = steward_proto::VERSION)]
struct Args {
    /// -v: roots, their settings and each phase of scans and hashing;
    /// -vv: also every file, request and event. RUST_LOG overrides.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args = Args::parse();
    steward_log::init(steward_log::Level::INFO, args.verbose, true);
    match stewardd::server::run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
