use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::sync::Mutex;
use tokio::runtime::Handle;
use tokio::task;
use tokio::time;

use bdk_chain::bitcoin::{constants, BlockHash, Transaction};

use bdk_chain::{
    collections::HashSet,
    indexed_tx_graph::{self, IndexedTxGraph},
    keychain,
    local_chain::{self, LocalChain},
    spk_client::FullScanResult,
    Append, ConfirmationTimeHeightAnchor,
};

use example_cli::{
    clap::{self, Args, Subcommand},
    handle_commands, Commands, Keychain,
};

use bdk_kyoto::logger::PrintLogger;
use bdk_kyoto::node::builder::NodeBuilder;
use bdk_kyoto::{HeaderCheckpoint, TxBroadcast, TxBroadcastPolicy};

type ChangeSet = (
    local_chain::ChangeSet,
    indexed_tx_graph::ChangeSet<ConfirmationTimeHeightAnchor, keychain::ChangeSet<Keychain>>,
);

/// Peer address whitelist
const PEERS: &[IpAddr] = &[
    IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
    IpAddr::V4(Ipv4Addr::new(170, 75, 163, 219)),
    IpAddr::V4(Ipv4Addr::new(23, 137, 57, 100)),
];
/// Bitcoin P2P port
const PORT: u16 = 38333;
/// Target derivation index in case none are revealed for a keychain.
const TARGET_INDEX: u32 = 20;

const DB_MAGIC: &[u8] = b"bdk_kyoto_example";
const DB_PATH: &str = ".bdk_kyoto_example.db";

#[derive(Debug, Clone, Subcommand)]
enum Cmd {
    /// Scan
    Scan {
        #[clap(flatten)]
        args: Arg,
    },
}

#[derive(Args, Debug, Clone)]
struct Arg {
    /// Start height
    #[clap(long)]
    height: Option<u32>,
    /// Start hash
    #[clap(long)]
    hash: Option<BlockHash>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let example_cli::Init {
        args,
        keymap,
        index,
        db,
        init_changeset,
    } = example_cli::init::<Cmd, Arg, ChangeSet>(DB_MAGIC, DB_PATH)?;

    let (init_chain_changeset, init_indexed_tx_graph_changeset) = init_changeset;
    let network = args.network;

    let (graph, anchor_heights) = {
        let mut graph = IndexedTxGraph::new(index);
        graph.apply_changeset(init_indexed_tx_graph_changeset);
        let heights: HashSet<_> = graph
            .graph()
            .all_anchors()
            .iter()
            .map(|(a, _)| a.confirmation_height)
            .collect();
        (Mutex::new(graph), heights)
    };

    let (chain, local_heights) = {
        let g = constants::genesis_block(args.network).block_hash();
        let (mut chain, _) = LocalChain::from_genesis_hash(g);
        chain.apply_changeset(&init_chain_changeset)?;
        let heights: HashSet<_> = chain.iter_checkpoints().map(|cp| cp.height()).collect();
        (Mutex::new(chain), heights)
    };

    let broadcast_fn = |_, tx: &Transaction| -> anyhow::Result<()> {
        let builder = NodeBuilder::new(network);
        let (mut node, client) = builder
            .add_peers(
                PEERS
                    .iter()
                    .cloned()
                    .map(|ip| (ip, Some(PORT)).into())
                    .collect(),
            )
            .num_required_peers(2)
            .build_node();

        let (sender, _) = client.split();

        task::spawn(async move { node.run().await });

        let sender_clone = sender.clone();
        Handle::current().block_on(async move {
            sender_clone
                .broadcast_tx(TxBroadcast {
                    tx: tx.clone(),
                    broadcast_policy: TxBroadcastPolicy::AllPeers,
                })
                .await
                .unwrap();
        });
        Handle::current().block_on(async move {
            sender.shutdown().await.unwrap();
        });

        Ok(())
    };

    // execute a general command, i.e. not a chain specific cmd
    if !matches!(args.command, Commands::ChainSpecific(_)) {
        let _ = handle_commands(
            &graph,
            &db,
            &chain,
            &keymap,
            args.network,
            broadcast_fn,
            args.command.clone(),
        );

        return Ok(());
    }

    let now = time::Instant::now();

    match args.command {
        Commands::ChainSpecific(cmd) => {
            match cmd {
                Cmd::Scan { args } => {
                    let mut spks = HashSet::new();

                    let header_cp = {
                        let chain = chain.lock().unwrap();
                        let graph = graph.lock().unwrap();

                        // Populate list of watched SPKs
                        let indexer = &graph.index;
                        for (keychain, _) in indexer.keychains() {
                            let last_reveal = indexer
                                .last_revealed_index(keychain)
                                .unwrap_or(TARGET_INDEX);
                            for index in 0..=last_reveal {
                                let spk = graph.index.spk_at_index(*keychain, index).unwrap();
                                spks.insert(spk.to_owned());
                            }
                        }

                        // Begin sync from a specified wallet birthday or else
                        // the last local checkpoint
                        let cp = chain.tip();
                        match (args.height, args.hash) {
                            (Some(height), Some(hash)) => HeaderCheckpoint::new(height, hash),
                            (None, None) => HeaderCheckpoint::new(cp.height(), cp.hash()),
                            _ => anyhow::bail!("missing one of --height or --hash"),
                        }
                    };

                    println!("Anchored at block {} {}", header_cp.height, header_cp.hash);

                    // Configure kyoto node
                    let builder = NodeBuilder::new(network);
                    let (mut node, client) = builder
                        .add_peers(
                            PEERS
                                .iter()
                                .cloned()
                                .map(|ip| (ip, Some(PORT)).into())
                                .collect(),
                        )
                        .add_scripts(spks)
                        .anchor_checkpoint(header_cp)
                        .num_required_peers(2)
                        .build_node();

                    let mut client = {
                        let chain = chain.lock().unwrap();
                        let graph = graph.lock().unwrap();

                        let mut client =
                            bdk_kyoto::Client::from_index(chain.tip(), &graph.index, client);
                        client.set_logger(Arc::new(PrintLogger::new()));
                        client
                    };

                    // Run the node
                    task::spawn(async move { node.run().await });

                    // Sync and apply updates
                    if let Some(res) = client.update().await {
                        let FullScanResult {
                            chain_update,
                            graph_update,
                            last_active_indices,
                        } = res;

                        let mut chain = chain.lock().unwrap();
                        let mut graph = graph.lock().unwrap();
                        let mut db = db.lock().unwrap();

                        let chain_changeset = chain.apply_update(chain_update)?;
                        let index_changeset =
                            graph.index.reveal_to_target_multi(&last_active_indices);
                        let mut indexed_graph_changeset = graph.apply_update(graph_update);
                        indexed_graph_changeset.append(index_changeset.into());

                        db.append_changeset(&(chain_changeset, indexed_graph_changeset))?;
                    }

                    client.shutdown().await?;

                    let elapsed = now.elapsed();
                    println!("Duration: {}s", elapsed.as_secs_f32());

                    let chain = chain.lock().unwrap();
                    let graph = graph.lock().unwrap();
                    let index = &graph.index;
                    let cp = chain.tip();
                    println!("Local tip: {} {}", cp.height(), cp.hash());
                    for (keychain, index) in index.last_revealed_indices() {
                        println!("Last revealed {keychain:?}: {index}");
                    }
                    let outpoints = index.outpoints().clone();
                    let balance =
                        graph
                            .graph()
                            .balance(&*chain, cp.block_id(), outpoints, |_, _| true);
                    println!("immature: {} sats", balance.immature.to_sat());
                    println!("trusted_pending: {} sats", balance.trusted_pending.to_sat());
                    println!(
                        "untrusted_pending: {} sats",
                        balance.untrusted_pending.to_sat()
                    );
                    println!("confirmed: {} sats", balance.confirmed.to_sat());
                    let update_heights: HashSet<_> =
                        chain.iter_checkpoints().map(|cp| cp.height()).collect();
                    assert!(
                        update_heights.is_superset(&local_heights),
                        "all heights in original chain must be present after update"
                    );
                    assert!(
                        update_heights.is_superset(&anchor_heights),
                        "all anchor heights must be present after update"
                    );
                }
            }
        }
        _ => unreachable!("handled above"),
    }

    Ok(())
}
