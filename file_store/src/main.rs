use clap::Parser;
use file_store::{
    cli::{bucket, dump, info},
    Result,
};

#[derive(Debug, clap::Subcommand)]
pub enum Cmd {
    Info(info::Cmd),
    Dump(dump::Cmd),
    Bucket(bucket::Cmd),
}

#[derive(Debug, clap::Parser)]
#[clap(version = env!("CARGO_PKG_VERSION"))]
#[clap(about = "Helium Bucket Commands")]
pub struct Cli {
    #[clap(subcommand)]
    cmd: Cmd,
}

#[tokio::main]
async fn main() -> Result {
    let cli = Cli::parse();

    match cli.cmd {
        Cmd::Info(cmd) => cmd.run().await,
        Cmd::Dump(cmd) => cmd.run().await,
        Cmd::Bucket(cmd) => cmd.run().await,
    }
}
