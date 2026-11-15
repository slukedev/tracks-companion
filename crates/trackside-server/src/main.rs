//! `trackside`: the Trackside music server.

use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = trackside_server::cli::Cli::parse();
    let code = trackside_server::cli::main(cli).await?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}
