use std::time::Duration;

use bolt_sidecar::{
    crypto::{
        bls::{from_bls_signature_to_consensus_signature, Signer, SignerBLS},
        SignableBLS,
    },
    json_rpc::{api::ApiError, start_server},
    primitives::{
        BatchedSignedConstraints, ChainHead, CommitmentRequest, ConstraintsMessage,
        LocalPayloadFetcher, SignedConstraints,
    },
    spec::ConstraintsApi,
    start_builder_proxy,
    state::{
        fetcher::{StateClient, StateFetcher},
        ExecutionState,
    },
    BuilderProxyConfig, Config, MevBoostClient,
};

use tokio::sync::mpsc;
use tracing::info;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();

    info!("Starting sidecar");

    let config = Config::parse_from_cli()?;

    let (api_events, mut api_events_rx) = mpsc::channel(1024);

    // TODO: support external signers
    let signer = Signer::new(config.private_key.clone().unwrap());

    let state_client = StateClient::new(&config.execution_api, 8);
    let mevboost_client = MevBoostClient::new(&config.mevboost_url);

    let head = state_client.get_head().await?;
    let mut execution_state = ExecutionState::new(state_client, ChainHead::new(0, head)).await?;

    let shutdown_tx = start_server(config, api_events).await?;

    let builder_proxy_config = BuilderProxyConfig::default();

    let (payload_tx, mut payload_rx) = mpsc::channel(1);
    let payload_fetcher = LocalPayloadFetcher::new(payload_tx);

    tokio::spawn(async move {
        loop {
            if let Err(e) =
                start_builder_proxy(payload_fetcher.clone(), builder_proxy_config.clone()).await
            {
                tracing::error!("Builder API proxy failed: {:?}", e);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    });

    // TODO: parallelize this
    loop {
        tokio::select! {
            Some(event) = api_events_rx.recv() => {
                tracing::info!("Received commitment request: {:?}", event.request);
                let request = event.request;

                if let Err(e) = execution_state
                    .try_commit(&CommitmentRequest::Inclusion(request.clone()))
                    .await
                {
                    tracing::error!("Failed to commit request: {:?}", e);
                    let _ = event.response.send(Err(ApiError::Custom(e.to_string())));
                    continue;
                }

                tracing::info!(
                    tx_hash = %request.tx.tx_hash(),
                    "Validation against execution state passed"
                );

                // parse the request into constraints and sign them with the sidecar signer
                // TODO: get the validator index from somewhere
                let message = ConstraintsMessage::build(0, request.slot, request.clone());

                let signature = from_bls_signature_to_consensus_signature(signer.sign(&message.digest())?);
                let signed_constraints: BatchedSignedConstraints =
                    vec![SignedConstraints { message, signature: signature.to_string() }];

                // TODO: fix retry logic
                while let Err(e) = mevboost_client
                    .submit_constraints(&signed_constraints)
                    .await
                {
                    tracing::error!(error = ?e, "Error submitting constraints, retrying...");
                }
            }
            Some(request) = payload_rx.recv() => {
                tracing::info!("Received payload request: {:?}", request);
                let Some(response) = execution_state.get_block_template(request.slot) else {
                    tracing::warn!("No block template found for slot {} when requested", request.slot);
                    let _ = request.response.send(None);
                    continue;
                };

                // For fallback block building, we need to turn a block template into an actual SignedBuilderBid.
                // This will also require building the full ExecutionPayload that we want the proposer to commit to.
                // Once we have that, we need to send it as response to the validator via the pending get_header RPC call.
                // The validator will then call get_payload with the corresponding SignedBlindedBeaconBlock. We then need to
                // respond with the full ExecutionPayload inside the BeaconBlock (+ blobs if any).

                let _ = request.response.send(None);
            }

            else => break,
        }
    }

    tokio::signal::ctrl_c().await?;
    shutdown_tx.send(()).await.ok();

    Ok(())
}
