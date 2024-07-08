use std::net::{IpAddr, Ipv4Addr};
use std::sync::Mutex;
use tokio::runtime::Handle;
use tokio::task;
use tokio::time;

use bdk_wallet::bitcoin::{constants, BlockHash, Transaction};

use bdk_wallet::chain::{
    collections::HashSet,
    indexed_tx_graph::{self, IndexedTxGraph},
    keychain,
    local_chain::{self, LocalChain},
    ConfirmationTimeHeightAnchor,
};

use example_cli::{
    clap::{self, Args, Subcommand},
    handle_commands, Commands, Keychain,
};

use kyoto::chain::checkpoints::HeaderCheckpoint;
use kyoto::node::builder::NodeBuilder;
use kyoto::{TxBroadcast, TxBroadcastPolicy};

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
    /// Sync
    Sync {
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
    let subscriber = tracing_subscriber::FmtSubscriber::new();
    tracing::subscriber::set_global_default(subscriber)?;

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
        let (mut node, mut client) = Handle::current().block_on(async move {
            builder
                .add_peers(
                    PEERS
                        .iter()
                        .cloned()
                        .map(|ip| (ip, Some(PORT)).into())
                        .collect(),
                )
                .num_required_peers(2)
                .build_node()
                .await
        });

        let (mut sender, _) = client.split();

        task::spawn(async move { node.run().await });

        let mut clone = sender.clone();
        Handle::current().block_on(async move {
            clone
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
                Cmd::Sync { args } => {
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

                    tracing::info!("Anchored at block {} {}", header_cp.height, header_cp.hash);

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
                        .build_node()
                        .await;

                    let mut client = {
                        let chain = chain.lock().unwrap();
                        let graph = graph.lock().unwrap();

                        let req = bdk_kyoto::Request::new(chain.tip(), &graph.index);
                        req.into_client(client)
                    };

                    // Run the node
                    task::spawn(async move { node.run().await });

                    // Sync and apply updates
                    if let Some(bdk_kyoto::Update {
                        cp,
                        indexed_tx_graph,
                    }) = client.update().await
                    {
                        let mut chain = chain.lock().unwrap();
                        let mut graph = graph.lock().unwrap();
                        let mut db = db.lock().unwrap();

                        let chain_changeset = chain.apply_update(cp)?;
                        let graph_changeset = indexed_tx_graph.initial_changeset();
                        graph.apply_changeset(graph_changeset.clone());
                        db.append_changeset(&(chain_changeset, graph_changeset))?;
                    }

                    client.shutdown().await?;

                    let elapsed = now.elapsed();
                    tracing::info!("Duration: {}s", elapsed.as_secs_f32());

                    let chain = chain.lock().unwrap();
                    let graph = graph.lock().unwrap();
                    let cp = chain.tip();
                    tracing::info!("Local tip: {} {}", cp.height(), cp.hash());
                    let outpoints = graph.index.outpoints().clone();
                    tracing::info!(
                        "Balance: {:#?}",
                        graph
                            .graph()
                            .balance(&*chain, cp.block_id(), outpoints, |_, _| true)
                    );
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
