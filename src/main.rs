//! This example shows how to run a custom dev node programmatically and submit a transaction
//! through rpc.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use std::{
    borrow::BorrowMut,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use alloy_genesis::Genesis;
use alloy_primitives::{b256, hex};
use futures_util::StreamExt;
use reth::{
    builder::{NodeBuilder, NodeHandle},
    providers::CanonStateSubscriptions,
    rpc::api::eth::helpers::EthTransactions,
    tasks::TaskManager,
};
use reth_chainspec::ChainSpec;
use reth_network::{NetworkInfo, PeersInfo};
use reth_node_core::{args::RpcServerArgs, node_config::NodeConfig, primitives::SignedTransaction};
use reth_node_ethereum::EthereumNode;
use reth_tracing::{
    tracing::{self, info_span, level_filters::LevelFilter, Instrument},
    LayerInfo, LogFormat, RethTracer, Tracer,
};

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
    let tasks = TaskManager::current();

    // create node config
    let mut config1 = NodeConfig::test()
        .dev()
        .with_rpc(RpcServerArgs::default().with_http().with_unused_ports())
        .with_chain(custom_chain());

    let NodeHandle {
        node: boot_node,
        node_exit_future: node_future,
    } = NodeBuilder::new(config1.clone())
        .testing_node(tasks.executor())
        .node(EthereumNode::default())
        .launch()
        .instrument(info_span!("node 1"))
        .await?;

    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let boot_addr = boot_node.network.local_node_record();
    tracing::info!(?boot_addr, "peer derived");
    let mut config2 = NodeConfig::test()
        .dev()
        .with_rpc(RpcServerArgs::default().with_http().with_unused_ports())
        .with_chain(custom_chain());

    // Tell the second node about the boot node
    config2.network.bootnodes = Some(vec![boot_addr.clone().into()]);
    config2.network.trusted_peers = vec![boot_addr.into()];

    let NodeHandle {
        node,
        node_exit_future: node_future_2,
    } = NodeBuilder::new(config2)
        .testing_node(tasks.executor())
        .node(EthereumNode::default())
        .launch()
        .instrument(info_span!("node 2"))
        .await?;

    tokio::select! {
        _ = node_future => {
            tracing::error!("node 1 crashed");
        }
        _ = node_future_2 => {
            tracing::error!("node 2 crashed");
        }
    }
    Ok(())
}

fn custom_chain() -> Arc<ChainSpec> {
    let custom_genesis = r#"
{
    "nonce": "0x42",
    "timestamp": "0x0",
    "extraData": "0x5343",
    "gasLimit": "0x1388",
    "difficulty": "0x400000000",
    "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
    "coinbase": "0x0000000000000000000000000000000000000000",
    "alloc": {
        "0x6Be02d1d3665660d22FF9624b7BE0551ee1Ac91b": {
            "balance": "0x4a47e3c12448f4ad000000"
        }
    },
    "number": "0x0",
    "gasUsed": "0x0",
    "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
    "config": {
        "ethash": {},
        "chainId": 2600,
        "homesteadBlock": 0,
        "eip150Block": 0,
        "eip155Block": 0,
        "eip158Block": 0,
        "byzantiumBlock": 0,
        "constantinopleBlock": 0,
        "petersburgBlock": 0,
        "istanbulBlock": 0,
        "berlinBlock": 0,
        "londonBlock": 0,
        "terminalTotalDifficulty": 0,
        "terminalTotalDifficultyPassed": true,
        "shanghaiTime": 0
    }
}
"#;
    let genesis: Genesis = serde_json::from_str(custom_genesis).unwrap();
    Arc::new(genesis.into())
}
