use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use log::{debug, info, warn};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use fan::tokio::mpsc::FanOut;
use std::time::Duration;
use futures::future::join_all;
use itertools::Itertools;
use quinn::{ClientConfig, Endpoint, EndpointConfig, IdleTimeout, TokioRuntime, TransportConfig, VarInt};
use solana_sdk::quic::QUIC_MAX_TIMEOUT_MS;
use solana_sdk::transaction::VersionedTransaction;
use solana_streamer::nonblocking::quic::ALPN_TPU_PROTOCOL_ID;
use solana_streamer::tls_certificates::new_self_signed_tls_certificate;
use tokio::sync::mpsc::{channel, Receiver};
use tokio::time::timeout;
use crate::quic_util::SkipServerVerification;
use crate::quinn_auto_reconnect::AutoReconnect;
use crate::shared::ForwardPacket;
use crate::validator_identity::ValidatorIdentity;

// takes transactions from upstream clients and forwards them to the TPU
pub async fn tx_forwarder(validator_identity: ValidatorIdentity, mut transaction_channel: Receiver<ForwardPacket>, exit_signal: Arc<AtomicBool>) -> anyhow::Result<()> {
    info!("TPU Quic forwarder started");

    let endpoint = new_endpoint_with_validator_identity(validator_identity).await;

    let mut agents: HashMap<SocketAddr, FanOut<ForwardPacket>> = HashMap::new();

    loop {
        // TODO add exit

        let forward_packet = transaction_channel.recv().await.expect("channel closed unexpectedly");
        let tpu_address = forward_packet.tpu_address;

        if !agents.contains_key(&tpu_address) {
            // TODO cleanup agent after a while of iactivity

            let mut senders = Vec::new();
            for _i in 0..4 {
                let (sender, mut receiver) = channel::<ForwardPacket>(100000);
                senders.push(sender);
                let exit_signal = exit_signal.clone();
                let endpoint_copy = endpoint.clone();
                tokio::spawn(async move {
                    debug!("Start Quic forwarder agent for TPU {}", tpu_address);
                    // TODO pass+check the tpu_address
                    // TODO connect
                    // TODO consume queue
                    // TODO exit signal

                    let auto_connection = AutoReconnect::new(endpoint_copy, tpu_address);
                    // let mut connection = tpu_quic_client_copy.create_connection(tpu_address).await.expect("handshake");
                    loop {

                        let _exit_signal = exit_signal.clone();
                        loop {
                            let packet = receiver.recv().await.unwrap();
                            assert_eq!(packet.tpu_address, tpu_address, "routing error");

                            let mut transactions_batch = packet.transactions;

                            let mut batch_size = 1;
                            while let Ok(more) = receiver.try_recv() {
                                transactions_batch.extend(more.transactions);
                                batch_size += 1;
                            }
                            if batch_size > 1 {
                                debug!("encountered batch of size {}", batch_size);
                            }

                            debug!("forwarding transaction batch of size {} to address {}", transactions_batch.len(), packet.tpu_address);

                            // TODo move send_txs_to_tpu_static to tpu_quic_client
                            let result = timeout(Duration::from_millis(500),
                                                 send_txs_to_tpu_static(&auto_connection, &transactions_batch)).await;
                            // .expect("timeout sending data to TPU node");

                            if result.is_err() {
                                warn!("send_txs_to_tpu_static result {:?} - loop over errors", result);
                            } else {
                                debug!("send_txs_to_tpu_static sent {}", transactions_batch.len());
                            }

                        }

                    }

                });

            }

            let fanout = FanOut::new(senders);

            agents.insert(tpu_address, fanout);

        } // -- new agent

        let agent_channel = agents.get(&tpu_address).unwrap();

        agent_channel.send(forward_packet).await.unwrap();

        // let mut batch_size = 1;
        // while let Ok(more) = transaction_channel.try_recv() {
        //     agent_channel.send(more).await.unwrap();
        //     batch_size += 1;
        // }
        // if batch_size > 1 {
        //     debug!("encountered batch of size {}", batch_size);
        // }


        // check if the tpu has already a task+queue running, if not start one, sort+queue packets by tpu address
        // maintain the health of a TPU connection, debounce errors; if failing, drop the respective messages

        // let exit_signal_copy = exit_signal.clone();
        // debug!("send transaction batch of size {} to address {}", forward_packet.transactions.len(), forward_packet.tpu_address);
        // // TODO: this will block/timeout if the TPU is not available
        // timeout(Duration::from_millis(500),
        //         tpu_quic_client_copy.send_txs_to_tpu(tpu_address, &forward_packet.transactions, exit_signal_copy)).await;
        // tpu_quic_client_copy.send_txs_to_tpu(forward_packet.tpu_address, &forward_packet.transactions, exit_signal_copy).await;

    } // -- loop over transactions from ustream channels

    // not reachable
}

/// takes a validator identity and creates a new QUIC client; appears as staked peer to TPU
// note: ATM the provided identity might or might not be a valid validator keypair
async fn new_endpoint_with_validator_identity(validator_identity: ValidatorIdentity) -> Endpoint {
    info!("Setup TPU Quic stable connection with validator identity {} ...", validator_identity);
    let (certificate, key) = new_self_signed_tls_certificate(
        &validator_identity.get_keypair_for_tls(),
        IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
    )
        .expect("Failed to initialize QUIC connection certificates");

    let endpoint_outbound = create_tpu_client_endpoint(certificate.clone(), key.clone());

    endpoint_outbound
}


const QUIC_CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);
pub const CONNECTION_RETRY_COUNT: usize = 10;

pub const MAX_TRANSACTIONS_PER_BATCH: usize = 10;
pub const MAX_BYTES_PER_BATCH: usize = 10;
const MAX_PARALLEL_STREAMS: usize = 6;

fn create_tpu_client_endpoint(certificate: rustls::Certificate, key: rustls::PrivateKey) -> Endpoint {
    let mut endpoint = {
        let client_socket =
            solana_net_utils::bind_in_range(IpAddr::V4(Ipv4Addr::UNSPECIFIED), (8000, 10000))
                .expect("create_endpoint bind_in_range")
                .1;
        let config = EndpointConfig::default();
        quinn::Endpoint::new(config, None, client_socket, TokioRuntime)
            .expect("create_endpoint quinn::Endpoint::new")
    };

    let mut crypto = rustls::ClientConfig::builder()
        .with_safe_defaults()
        .with_custom_certificate_verifier(SkipServerVerification::new())
        .with_single_cert(vec![certificate], key)
        .expect("Failed to set QUIC client certificates");

    crypto.enable_early_data = true;

    crypto.alpn_protocols = vec![ALPN_TPU_PROTOCOL_ID.to_vec()];

    let mut config = ClientConfig::new(Arc::new(crypto));

    // note: this should be aligned with solana quic server's endpoint config
    let mut transport_config = TransportConfig::default();
    // no remotely-initiated streams required
    transport_config.max_concurrent_uni_streams(VarInt::from_u32(0));
    transport_config.max_concurrent_bidi_streams(VarInt::from_u32(0));
    let timeout = IdleTimeout::try_from(Duration::from_millis(QUIC_MAX_TIMEOUT_MS as u64)).unwrap();
    transport_config.max_idle_timeout(Some(timeout));
    transport_config.keep_alive_interval(None);
    config.transport_config(Arc::new(transport_config));

    endpoint.set_default_client_config(config);

    endpoint
}

fn serialize_to_vecvec(transactions: &Vec<VersionedTransaction>) -> Vec<Vec<u8>> {
    transactions.iter().map(|tx| {
        let tx_raw = bincode::serialize(tx).unwrap();
        tx_raw
    }).collect_vec()
}


// send potentially large amount of transactions to a single TPU
#[tracing::instrument(skip_all, level = "debug")]
async fn send_txs_to_tpu_static(
    auto_connection: &AutoReconnect,
    txs: &Vec<VersionedTransaction>,
) {

    // note: this impl does not deal with connection errors
    // throughput_50 493.70 tps
    // throughput_50 769.43 tps (with finish timeout)
    // TODO join get_or_create_connection future and read_to_end
    // TODO add error handling

    for chunk in txs.chunks(MAX_PARALLEL_STREAMS) {
        let all_send_fns = chunk.iter().map(|tx| {
            let tx_raw = bincode::serialize(tx).unwrap();
            tx_raw
        })
            .map(|tx_raw| {
                auto_connection.send(tx_raw) // ignores error
            });

        // let all_send_fns = (0..txs.len()).map(|i| auto_connection.roundtrip(vecvec.get(i))).collect_vec();

        join_all(all_send_fns).await;

    }

}
