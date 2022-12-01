use {
    crate::{Error, Router},
    crossbeam_channel::{Receiver, Sender},
    itertools::Itertools,
    log::{error, info, trace},
    rand::{seq::SliceRandom, Rng},
    solana_client::{
        rpc_client::RpcClient, rpc_config::RpcGetVoteAccountsConfig,
        rpc_response::RpcVoteAccountStatus,
    },
    solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey},
    std::{
        borrow::Borrow,
        cmp::Reverse,
        collections::{hash_map::Entry, HashMap, HashSet},
        iter::repeat_with,
        str::FromStr,
        time::{Duration, Instant},
    },
};

#[derive(Debug)]
pub struct Node {
    clock: Instant,
    num_gossip_rounds: usize,
    pubkey: Pubkey,
    stake: u64,
    table: HashMap<CrdsKey, /*ordinal:*/ u64>,
    receiver: Receiver<Packet>,
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub gossip_push_fanout: usize,
    // Maximum number of packets to push in each gossip round.
    pub gossip_push_capacity: usize,
    pub packet_drop_rate: f64,
    pub num_crds: usize, // Number of crds values per node.
    // Num of crds values generated by each node in each gossip round.
    pub refresh_rate: f64,
    pub num_threads: usize,
    pub run_duration: Duration,
    // Number of gossip rounds before collecting stats.
    pub warm_up_rounds: usize,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CrdsKey {
    origin: Pubkey,
    index: usize,
}

#[derive(Clone, Copy)]
pub struct Packet {
    key: CrdsKey,
    ordinal: u64,
}

// TODO: should let nodes maintain their own view of the cluster?!
// TODO: gossip loop 200ms delay!? listen vs gossip!?

impl Node {
    pub fn stake(&self) -> u64 {
        self.stake
    }

    pub fn pubkey(&self) -> Pubkey {
        self.pubkey
    }

    pub fn table(&self) -> &HashMap<CrdsKey, /*ordinal:*/ u64> {
        &self.table
    }

    pub fn num_gossip_rounds(&self) -> usize {
        self.num_gossip_rounds
    }

    pub fn run_gossip<R: Rng>(
        &mut self,
        rng: &mut R,
        config: &Config,
        stakes: &HashMap<Pubkey, u64>,
        router: &Router<Packet>,
    ) -> Result<(), Error> {
        let elapsed = self.clock.elapsed();
        self.clock = Instant::now();
        self.num_gossip_rounds += 1;
        // Drain the channel for incomming packets.
        // Insert new messages into the CRDS table.
        let (mut keys, num_packets, num_outdated) = self.consume_packets();
        // Refresh own gossip entries!
        let num_refresh =
            config.refresh_rate as usize + rng.gen_bool(config.refresh_rate % 1.0) as usize;
        for index in repeat_with(|| rng.gen_range(0, config.num_crds)).take(num_refresh) {
            let key = CrdsKey {
                origin: self.pubkey,
                index,
            };
            let ordinal = self
                .table
                .get(&key)
                .map(|ordinal| ordinal + 1)
                .unwrap_or_default();
            self.table.insert(key, ordinal);
            keys.insert(key);
        }
        // Sort updated keys by origin's stake.
        let keys: Vec<_> = keys
            .into_iter()
            .map(|key| {
                let stake = stakes.get(&key.origin).copied().unwrap_or_default();
                (stake, key)
            })
            .sorted_unstable_by_key(|(stake, _)| Reverse(*stake))
            .map(|(_stake, key)| key)
            .collect();
        let num_keys = keys.len();
        // Push/fanout overwritten keys to other nodes.
        let mut nodes: Vec<_> = stakes.keys().copied().collect();
        nodes.shuffle(rng);
        for key in keys {
            let packet = Packet {
                key,
                ordinal: self.table[&key],
            };
            for _ in 0..config.gossip_push_fanout {
                // TODO: This may choose duplicate nodes!
                if let Some(node) = nodes.choose(rng) {
                    router.send(rng, node, packet)?;
                }
            }
        }
        trace!(
            "{}, {:?}: {}ms, packets: {}, outdated: {}, {:.0}%, keys: {}, {}ms",
            &format!("{}", self.pubkey)[..8],
            std::thread::current().id(),
            elapsed.as_millis(),
            num_packets,
            num_outdated,
            if num_packets == 0 {
                0.0
            } else {
                num_outdated as f64 * 100.0 / num_packets as f64
            },
            num_keys,
            self.clock.elapsed().as_millis(),
        );
        Ok(())
    }

    /// Drains the channel for incoming packets and updates crds table.
    pub fn consume_packets(
        &mut self,
    ) -> (
        HashSet<CrdsKey>,
        usize, // num packets
        usize, // num outdated
    ) {
        let packets: Vec<_> = self.receiver.try_iter().collect();
        // Insert new messages into the CRDS table.
        let mut keys = HashSet::<CrdsKey>::new();
        let num_packets = packets.len();
        let mut num_outdated = 0;
        for Packet { key, ordinal } in packets {
            match self.table.entry(key) {
                Entry::Occupied(mut entry) => {
                    if entry.get() < &ordinal {
                        entry.insert(ordinal);
                        keys.insert(key);
                    } else {
                        num_outdated += 1;
                    }
                }
                Entry::Vacant(entry) => {
                    entry.insert(ordinal);
                    keys.insert(key);
                }
            }
        }
        (keys, num_packets, num_outdated)
    }
}

pub fn make_gossip_cluster(rpc_client: &RpcClient) -> Result<Vec<(Node, Sender<Packet>)>, Error> {
    let config = RpcGetVoteAccountsConfig {
        vote_pubkey: None,
        commitment: Some(CommitmentConfig::finalized()),
        keep_unstaked_delinquents: Some(true),
        delinquent_slot_distance: None,
    };
    let vote_accounts: RpcVoteAccountStatus = rpc_client.get_vote_accounts_with_config(config)?;
    info!(
        "num of vote accounts: {}",
        vote_accounts.current.len() + vote_accounts.delinquent.len()
    );
    let stakes: HashMap</*node pubkey:*/ String, /*activated stake:*/ u64> = vote_accounts
        .current
        .iter()
        .chain(&vote_accounts.delinquent)
        .into_grouping_map_by(|info| info.node_pubkey.clone())
        .aggregate(|stake, _node_pubkey, vote_account_info| {
            Some(stake.unwrap_or_default() + vote_account_info.activated_stake)
        });
    info!("num of node pubkeys in vote accounts: {}", stakes.len());
    let nodes = rpc_client.get_cluster_nodes()?;
    let shred_versions: HashSet<_> = nodes.iter().map(|node| node.shred_version).collect();
    if shred_versions.len() > 1 {
        error!("multiple shred versions: {:?}", shred_versions);
    } else {
        info!("shred versions: {:?}", shred_versions);
    }
    let now = Instant::now();
    let nodes: Vec<_> = nodes
        .into_iter()
        .map(|node| {
            let stake = stakes.get(&node.pubkey).copied().unwrap_or_default();
            let pubkey = Pubkey::from_str(&node.pubkey)?;
            let (sender, receiver) = crossbeam_channel::unbounded();
            let node = Node {
                clock: now,
                num_gossip_rounds: 0,
                stake,
                pubkey,
                table: HashMap::default(),
                receiver,
            };
            Ok((node, sender))
        })
        .collect::<Result<_, Error>>()?;
    let num_nodes_staked = nodes
        .iter()
        .filter(|(node, _sender)| node.stake != 0)
        .count();
    info!("num of staked nodes in cluster: {}", num_nodes_staked);
    info!("num of cluster nodes: {}", nodes.len());
    let active_stake: u64 = stakes.values().sum();
    let cluster_stake: u64 = nodes.iter().map(|(node, _sender)| node.stake).sum();
    info!("active stake:  {}", active_stake);
    info!("cluster stake: {}", cluster_stake);
    Ok(nodes)
}

/// Returns most recent CRDS table across all nodes.
pub fn get_crds_table<I, T>(nodes: I) -> HashMap<CrdsKey, /*ordinal:*/ u64>
where
    I: IntoIterator<Item = T>,
    T: Borrow<Node>,
{
    let mut out = HashMap::<CrdsKey, /*ordinal:*/ u64>::new();
    for node in nodes {
        for (key, ordinal) in node.borrow().table() {
            let entry = out.entry(*key).or_default();
            *entry = u64::max(*entry, *ordinal);
        }
    }
    out
}
