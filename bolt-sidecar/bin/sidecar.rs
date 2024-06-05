use bolt_sidecar::{
    config::{Config, Opts},
    json_rpc::start_server,
    start_builder_proxy, BuilderProxyConfig, NoopPayloadFetcher,
};

use clap::Parser;
use tracing::info;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();

    info!("Starting sidecar");

    let opts = Opts::parse();
    let config = Config::try_from(opts)?;

    let shutdown_tx = start_server(config).await?;

    let builder_proxy_config = BuilderProxyConfig::default();

    let builder_proxy = tokio::spawn(async move {
        if let Err(e) = start_builder_proxy(NoopPayloadFetcher, builder_proxy_config).await {
            tracing::error!("Builder proxy failed: {:?}", e);
        }
    });

    tokio::signal::ctrl_c().await?;
    shutdown_tx.send(()).await.ok();

    // High-level flow:
    // - Create block template
    // - Create state with client
    // - Subscribe to new blocks
    // - Update state on every new block
    // - Run template through state to invalidate commitments
    // - Accept new preconfs etc.

    Ok(())
}
