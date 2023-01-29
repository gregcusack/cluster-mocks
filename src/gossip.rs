use {
    crate::{push_active_set::PushActiveSet, received_cache::ReceivedCache, Error, Router},
    crossbeam_channel::{Receiver, Sender},
    itertools::Itertools,
    log::{error, info, trace},
    rand::Rng,
    solana_client::{
        rpc_client::RpcClient, rpc_config::RpcGetVoteAccountsConfig,
        rpc_response::RpcVoteAccountStatus,
    },
    solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey},
    std::{
        borrow::Borrow,
        cmp::{Ordering, Reverse},
        collections::{hash_map::Entry, HashMap, HashSet},
        iter::{repeat, repeat_with},
        str::FromStr,
        sync::Arc,
        time::{Duration, Instant},
    },
};

pub(crate) const CRDS_UNIQUE_PUBKEY_CAPACITY: usize = 8192;
const CRDS_GOSSIP_PRUNE_STAKE_THRESHOLD_PCT: f64 = 0.15;

pub struct Node {
    clock: Instant,
    num_gossip_rounds: usize,
    pubkey: Pubkey,
    stake: u64,
    table: HashMap<CrdsKey, CrdsEntry>,
    active_set: PushActiveSet,
    received_cache: ReceivedCache,
    receiver: Receiver<Arc<Packet>>,
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub gossip_push_fanout: f64,
    pub gossip_push_wide_fanout: f64,
    // Number of gossip rounds between push active set rotations.
    pub rotate_active_set_rounds: usize,
    // Min ingress number of nodes to keep when pruning received-cache.
    pub gossip_prune_min_ingress_nodes: usize,
    // TODO: wide fanout
    // TODO: Maximum number of packets to push in each gossip round.
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

#[derive(Debug, Default)]
pub struct CrdsEntry {
    ordinal: u64,
    num_dups: u8,
}

#[derive(Clone)]
pub enum Packet {
    Push {
        from: Pubkey,
        key: CrdsKey,
        ordinal: u64,
    },
    Prune {
        from: Pubkey,
        origins: Vec<Pubkey>,
    },
}

#[derive(Default)]
pub struct ConsumeOutput {
    keys: HashSet<CrdsKey>, // upserted keys
    num_packets: usize,
    num_prunes: usize,
    num_outdated: usize,
    num_duplicates: usize,
}

enum UpsertError {
    Outdated,
    Duplicate(/*num_dups:*/ u8),
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

    pub fn table(&self) -> &HashMap<CrdsKey, CrdsEntry> {
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
        router: &Router<Arc<Packet>>,
    ) -> Result<(), Error> {
        let elapsed = self.clock.elapsed();
        self.clock = Instant::now();
        self.num_gossip_rounds += 1;
        if self.num_gossip_rounds % config.rotate_active_set_rounds == 1 {
            self.rotate_active_set(rng, config.gossip_push_fanout as usize, stakes);
        }
        // Drain the channel for incomming packets.
        // Insert new messages into the CRDS table.
        let ConsumeOutput {
            mut keys,
            num_packets,
            num_prunes,
            num_outdated,
            num_duplicates,
        } = self.consume_packets(stakes);
        // Send prune messages for upserted origins.
        {
            let origins = keys.iter().map(|key| key.origin);
            self.send_prunes(rng, origins, config, stakes, router)?;
        }
        // Refresh own gossip entries!
        keys.extend(self.refresh_entries(rng, config));
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
        for key in keys {
            let packet = Arc::new(Packet::Push {
                from: self.pubkey,
                key,
                ordinal: self.table[&key].ordinal,
            });
            let gossip_push_fanout = if key.origin == self.pubkey {
                config.gossip_push_wide_fanout
            } else {
                config.gossip_push_fanout
            };
            let gossip_push_fanout =
                gossip_push_fanout as usize + rng.gen_bool(gossip_push_fanout % 1.0) as usize;
            for node in self
                .active_set
                .get_nodes(&self.pubkey, &key.origin, |_| false, stakes)
                .take(gossip_push_fanout)
            {
                assert_ne!(node, &self.pubkey);
                router.send(rng, node, packet.clone())?;
            }
        }
        let get_ratio = |num| {
            if num_packets == num_prunes {
                0.0
            } else {
                num as f64 * 100.0 / (num_packets - num_prunes) as f64
            }
        };
        if rng.gen_ratio(1, 1000) {
            trace!(
                "{}, {:?}: {}ms, round: {}, packets: {}, prunes: {},\
                outdated: {}, {:.0}%, duplicates: {}, {:.0}%, keys: {}, {}ms",
                &format!("{}", self.pubkey)[..8],
                std::thread::current().id(),
                elapsed.as_millis(),
                self.num_gossip_rounds,
                num_packets,
                num_prunes,
                num_outdated,
                get_ratio(num_outdated),
                num_duplicates,
                get_ratio(num_duplicates),
                num_keys,
                self.clock.elapsed().as_millis(),
            );
        }
        Ok(())
    }

    fn send_prunes<R: Rng>(
        &mut self,
        rng: &mut R,
        origins: impl IntoIterator<Item = Pubkey>, // upserted origins
        config: &Config,
        stakes: &HashMap<Pubkey, u64>,
        router: &Router<Arc<Packet>>,
    ) -> Result<(), Error> {
        let prunes = origins
            .into_iter()
            .flat_map(|origin| {
                self.received_cache
                    .prune(
                        &self.pubkey,
                        origin,
                        CRDS_GOSSIP_PRUNE_STAKE_THRESHOLD_PCT,
                        config.gossip_prune_min_ingress_nodes,
                        stakes,
                    )
                    .zip(repeat(origin))
            })
            .into_group_map();
        for (node, origins) in prunes {
            let packet = Packet::Prune {
                from: self.pubkey,
                origins,
            };
            router.send(rng, &node, Arc::new(packet))?;
        }
        Ok(())
    }

    // Refreshes own gossip entries, returning upserted crds keys.
    fn refresh_entries<'a, R: Rng>(
        &'a mut self,
        rng: &'a mut R,
        config: &'a Config,
    ) -> impl Iterator<Item = CrdsKey> + 'a {
        let num_refresh =
            config.refresh_rate as usize + rng.gen_bool(config.refresh_rate % 1.0) as usize;
        repeat_with(|| rng.gen_range(0, config.num_crds))
            .take(num_refresh)
            .map(|index| {
                let key = CrdsKey {
                    origin: self.pubkey,
                    index,
                };
                self.table.entry(key).or_default().ordinal += 1;
                key
            })
    }

    /// Drains the channel for incoming packets and updates crds table.
    pub fn consume_packets(&mut self, stakes: &HashMap<Pubkey, u64>) -> ConsumeOutput {
        let packets: Vec<_> = self.receiver.try_iter().collect();
        // Insert new messages into the CRDS table.
        let mut out = ConsumeOutput {
            num_packets: packets.len(),
            ..ConsumeOutput::default()
        };
        for packet in packets {
            match *packet {
                Packet::Push { from, key, ordinal } => {
                    match self.upsert(key, ordinal) {
                        Ok(()) => {
                            self.received_cache
                                .record(key.origin, from, /*num_dups:*/ 0);
                            out.keys.insert(key);
                        }
                        Err(UpsertError::Outdated) => {
                            self.received_cache.record(
                                key.origin,
                                from,
                                usize::MAX, // num_dups
                            );
                            out.num_outdated += 1;
                        }
                        Err(UpsertError::Duplicate(num_dups)) => {
                            self.received_cache
                                .record(key.origin, from, usize::from(num_dups));
                            out.num_duplicates += 1;
                        }
                    }
                }
                Packet::Prune {
                    ref from,
                    ref origins,
                } => {
                    out.num_prunes += 1;
                    self.active_set.prune(&self.pubkey, from, origins, stakes);
                }
            }
        }
        out
    }

    fn upsert(&mut self, key: CrdsKey, ordinal: u64) -> Result<(), UpsertError> {
        match self.table.entry(key) {
            Entry::Occupied(mut entry) => {
                let entry = entry.get_mut();
                match entry.ordinal.cmp(&ordinal) {
                    Ordering::Less => {
                        *entry = CrdsEntry {
                            ordinal,
                            num_dups: 0u8,
                        };
                        Ok(())
                    }
                    Ordering::Equal => {
                        entry.num_dups = entry.num_dups.saturating_add(1u8);
                        Err(UpsertError::Duplicate(entry.num_dups))
                    }
                    Ordering::Greater => Err(UpsertError::Outdated),
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(CrdsEntry {
                    ordinal,
                    num_dups: 0u8,
                });
                Ok(())
            }
        }
    }

    fn rotate_active_set<R: Rng>(
        &mut self,
        rng: &mut R,
        gossip_push_fanout: usize,
        stakes: &HashMap<Pubkey, u64>,
    ) {
        // Gossip nodes to be sampled for each push active set.
        // TODO: this should only be a set of entrypoints not all staked nodes.
        let nodes: Vec<_> = stakes
            .keys()
            .copied()
            .chain(self.table.keys().map(|key| key.origin))
            .filter(|pubkey| pubkey != &self.pubkey)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let cluster_size = nodes.len();
        self.active_set
            .rotate(rng, gossip_push_fanout * 3, cluster_size, &nodes, stakes);
    }
}

impl CrdsEntry {
    pub fn ordinal(&self) -> u64 {
        self.ordinal
    }
}

#[allow(clippy::type_complexity)]
pub fn make_gossip_cluster(
    rpc_client: &RpcClient,
) -> Result<Vec<(Node, Sender<Arc<Packet>>)>, Error> {
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
                active_set: PushActiveSet::default(),
                received_cache: ReceivedCache::new(2 * CRDS_UNIQUE_PUBKEY_CAPACITY),
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
        for (key, entry) in node.borrow().table() {
            let ordinal = out.entry(*key).or_default();
            *ordinal = u64::max(*ordinal, entry.ordinal);
        }
    }
    out
}
