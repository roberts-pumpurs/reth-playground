//! Low level example of connecting to and communicating with a peer.
//!
//! Run with
//!
//! ```not_rust
//! cargo run -p manual-p2p
//! ```

use std::{env, net::SocketAddr, time::Duration};

use alloy_consensus::constants::MAINNET_GENESIS_HASH;
use eyre::OptionExt;
use futures::StreamExt;
use reth_chainspec::{Chain, MAINNET};
use reth_discv4::{DiscoveryUpdate, Discv4, Discv4ConfigBuilder, DEFAULT_DISCOVERY_ADDRESS};
use reth_ecies::stream::ECIESStream;
use reth_eth_wire::{
    EthMessage, EthStream, HelloMessage, P2PStream, Status, UnauthedEthStream, UnauthedP2PStream,
};
use reth_network::{config::rng_secret_key, EthNetworkPrimitives};
use reth_network_peers::{mainnet_nodes, parse_nodes, pk2id, NodeRecord, PeerId};
use reth_primitives::{EthereumHardfork, Head};
use reth_tracing::{
    tracing::{error, info, level_filters::LevelFilter},
    LayerInfo, LogFormat, RethTracer, Tracer,
};
use secp256k1::{SecretKey, SECP256K1};
use std::sync::LazyLock;
use tokio::net::{TcpListener, TcpStream};

type AuthedP2PStream = P2PStream<ECIESStream<TcpStream>>;
type AuthedEthStream = EthStream<P2PStream<ECIESStream<TcpStream>>, EthNetworkPrimitives>;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let _ = RethTracer::new()
        .with_stdout(LayerInfo::new(
            LogFormat::Terminal,
            LevelFilter::INFO.to_string(),
            "".to_string(),
            Some("always".to_string()),
        ))
        .init();
    // ---------------
    // Setup local config
    // ---------------
    let our_key = rng_secret_key();
    let bind_ip = env::var("BIND_IP").unwrap_or_else(|_| "127.0.0.1".to_string());
    let bind_port = env::var("BIND_PORT").unwrap_or_else(|_| "30303".to_string());
    let local_addr: SocketAddr = format!("{bind_ip}:{bind_port}").parse()?;

    // Create local ENR from our key
    let our_enr = NodeRecord::from_secret_key(local_addr, &our_key);
    info!(?our_enr, "enr");

    // ---------------
    // Optionally connect to a peer from env
    // ---------------
    let maybe_peer_addr = env::var("PEER_ADDRESS").ok();
    let maybe_peer_port = env::var("PEER_TCP_PORT").ok();
    let maybe_peer_public_key = env::var("PEER_ID").ok();

    // Build a new Discv4 config using our local ENR and secret key.
    let discv4_cfg = Discv4ConfigBuilder::default()
        .add_boot_nodes(vec![])
        .lookup_interval(Duration::from_secs(1))
        .build();

    // Start the discovery service. The `udp_addr` is where we'll listen for discv4,
    // typically the same IP but it’s a UDP port.
    let discv4 = Discv4::spawn(our_enr.udp_addr(), our_enr.clone(), our_key, discv4_cfg).await?;

    // talk to the external peer
    if let (Some(peer_ip), Some(peer_port)) = (maybe_peer_addr, maybe_peer_port) {
        let peer_addr: SocketAddr = format!("{peer_ip}:{peer_port}").parse()?;
        let peer_id = maybe_peer_public_key
            .ok_or_eyre("expected peer pubkey")?
            .parse::<PeerId>()?;
        let peer = NodeRecord {
            address: peer_addr.ip(),
            tcp_port: peer_addr.port(),
            udp_port: peer_addr.port(),
            id: peer_id,
        };
        let key_clone = our_key.clone();

        // Create a stream that notifies us of newly discovered nodes.
        let mut discv4_stream = discv4.update_stream().await?;
        discv4.add_node(peer);

        // In a task, handle newly discovered peers:
        tokio::spawn(async move {
            while let Some(update) = discv4_stream.next().await {
                if let DiscoveryUpdate::Added(peer) = update {
                    // Optionally skip peers you already know, or skip if the peer is yourself, etc.

                    // Then connect (outbound) to them:
                    let (p2p_stream, their_hello) =
                        match outbound::handshake_p2p(peer, key_clone).await {
                            Ok(s) => s,
                            Err(e) => {
                                error!("Failed P2P handshake with discovered peer {:?}: {e}", peer);
                                continue;
                            }
                        };
                    // Now do eth-wire handshake, etc.
                    let (eth_stream, their_status) = match outbound::handshake_eth(p2p_stream).await
                    {
                        Ok(s) => s,
                        Err(e) => {
                            error!("Failed ETH handshake with peer {}, {}", peer.address, e);
                            return;
                        }
                    };

                    info!(
                        "Successfully connected to a peer at {}:{} ({}) using eth-wire version eth/{}",
                        peer.address, peer.tcp_port, their_hello.client_version, their_status.version
                    );

                    snoop(peer, eth_stream).await;
                }
            }
            unreachable!()
        });
    }

    // ---------------
    // Accept inbound connections
    // ---------------
    let listener = TcpListener::bind(local_addr).await?;
    info!("Listening on {local_addr}");
    loop {
        let (stream, remote_addr) = listener.accept().await?;
        info!("Inbound connection from {remote_addr}");
        let key = our_key.clone();

        // handle inbound in background
        tokio::spawn(async move {
            if let Err(e) = inbound::handle_inbound(stream, key).await {
                error!("Inbound connection error: {e}");
            }
        });
    }
}

mod outbound {
    use super::*;

    // Perform a P2P handshake with a peer
    pub async fn handshake_p2p(
        peer: NodeRecord,
        key: SecretKey,
    ) -> eyre::Result<(AuthedP2PStream, HelloMessage)> {
        let outgoing = TcpStream::connect((peer.address, peer.tcp_port)).await?;
        let ecies_stream = ECIESStream::connect(outgoing, key, peer.id).await?;

        let our_peer_id = pk2id(&key.public_key(SECP256K1));
        let our_hello = HelloMessage::builder(our_peer_id).build();

        Ok(UnauthedP2PStream::new(ecies_stream)
            .handshake(our_hello)
            .await?)
    }

    // Perform a ETH Wire handshake with a peer
    pub async fn handshake_eth(
        p2p_stream: AuthedP2PStream,
    ) -> eyre::Result<(AuthedEthStream, Status)> {
        let fork_filter = MAINNET.fork_filter(Head {
            timestamp: MAINNET
                .fork(EthereumHardfork::Shanghai)
                .as_timestamp()
                .unwrap(),
            ..Default::default()
        });

        let status = Status::builder()
            .chain(Chain::mainnet())
            .genesis(MAINNET_GENESIS_HASH)
            .forkid(
                MAINNET
                    .hardfork_fork_id(EthereumHardfork::Shanghai)
                    .unwrap(),
            )
            .build();

        let status = Status {
            version: p2p_stream
                .shared_capabilities()
                .eth()?
                .version()
                .try_into()?,
            ..status
        };
        let eth_unauthed = UnauthedEthStream::new(p2p_stream);
        Ok(eth_unauthed.handshake(status, fork_filter).await?)
    }
}

mod inbound {
    use outbound::handshake_eth;
    use reth_tracing::tracing::info;

    use super::*;

    /// Handle inbound connections: do the P2P handshake in server mode, then do ETH handshake, then snoop.
    pub async fn handle_inbound(stream: TcpStream, key: SecretKey) -> eyre::Result<()> {
        info!("processing inbound");
        let (p2p_stream, their_hello) = handshake_p2p_inbound(stream, key).await?;
        info!("p2p stream created");
        let (eth_stream, their_status) = handshake_eth(p2p_stream).await?;
        info!(
            "Inbound: handshake ok with client_version={} (eth/{}).",
            their_hello.client_version, their_status.version
        );
        // // If you want to keep reading messages from them, call `snoop`, else you can close.
        // // We'll just run snoop in the background for demonstration:
        // let peer = NodeRecord {
        //     address: Default::default(),
        //     tcp_port: 0,
        //     udp_port: 0,
        //     id: Default::default(),
        // };
        // snoop(peer, eth_stream).await;
        Ok(())
    }

    // Inbound P2P handshake
    async fn handshake_p2p_inbound(
        inbound_stream: TcpStream,
        key: SecretKey,
    ) -> eyre::Result<(AuthedP2PStream, HelloMessage)> {
        dbg!();
        let ecies_stream = ECIESStream::incoming(inbound_stream, key).await?;
        dbg!();
        let our_peer_id = pk2id(&key.public_key(SECP256K1));
        dbg!();
        let our_hello = HelloMessage::builder(our_peer_id).build();
        Ok(UnauthedP2PStream::new(ecies_stream)
            .handshake(our_hello)
            .await?)
    }
}

// Snoop by greedily capturing all broadcasts that the peer emits
// note: this node cannot handle request so will be disconnected by peer when challenged
async fn snoop(peer: NodeRecord, mut eth_stream: AuthedEthStream) {
    while let Some(Ok(update)) = eth_stream.next().await {
        match update {
            EthMessage::NewPooledTransactionHashes66(txs) => {
                info!(
                    "Got {} new tx hashes from peer {}",
                    txs.0.len(),
                    peer.address
                );
            }
            EthMessage::NewBlock(block) => {
                info!("Got new block data {:?} from peer {}", block, peer.address);
            }
            EthMessage::NewPooledTransactionHashes68(txs) => {
                info!(
                    "Got {} new tx hashes from peer {}",
                    txs.hashes.len(),
                    peer.address
                );
            }
            EthMessage::NewBlockHashes(block_hashes) => {
                info!(
                    "Got {} new block hashes from peer {}",
                    block_hashes.0.len(),
                    peer.address
                );
            }
            EthMessage::GetNodeData(_) => {
                info!(
                    "Unable to serve GetNodeData request to peer {}",
                    peer.address
                );
            }
            EthMessage::GetReceipts(_) => {
                info!(
                    "Unable to serve GetReceipts request to peer {}",
                    peer.address
                );
            }
            EthMessage::GetBlockHeaders(_) => {
                info!(
                    "Unable to serve GetBlockHeaders request to peer {}",
                    peer.address
                );
            }
            EthMessage::GetBlockBodies(_) => {
                info!(
                    "Unable to serve GetBlockBodies request to peer {}",
                    peer.address
                );
            }
            EthMessage::GetPooledTransactions(_) => {
                info!(
                    "Unable to serve GetPooledTransactions request to peer {}",
                    peer.address
                );
            }
            _ => {}
        }
    }
}
