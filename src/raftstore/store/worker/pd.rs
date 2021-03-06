// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;
use std::fmt::{self, Display, Formatter};

use futures::Future;
use tokio_core::reactor::Handle;

use kvproto::metapb;
use kvproto::eraftpb::ConfChangeType;
use kvproto::raft_cmdpb::{AdminCmdType, AdminRequest, RaftCmdRequest};
use kvproto::raft_serverpb::RaftMessage;
use kvproto::pdpb;
use rocksdb::DB;
use fs2;

use util::worker::FutureRunnable as Runnable;
use util::escape;
use util::transport::SendCh;
use util::rocksdb::*;
use pd::{PdClient, RegionStat};
use raftstore::store::Msg;
use raftstore::store::util::{get_region_approximate_size, is_epoch_stale};
use raftstore::store::metrics::*;
use raftstore::store::store::StoreInfo;
use raftstore::store::Callback;
use super::metrics::*;

// Use an asynchronous thread to tell pd something.
pub enum Task {
    AskSplit {
        region: metapb::Region,
        split_key: Vec<u8>,
        peer: metapb::Peer,
        // If true, right region derive origin region_id.
        right_derive: bool,
        callback: Option<Callback>,
    },
    Heartbeat {
        region: metapb::Region,
        peer: metapb::Peer,
        down_peers: Vec<pdpb::PeerStats>,
        pending_peers: Vec<metapb::Peer>,
        written_bytes: u64,
        written_keys: u64,
        read_bytes: u64,
        read_keys: u64,
    },
    StoreHeartbeat {
        stats: pdpb::StoreStats,
        store_info: StoreInfo,
    },
    ReportSplit {
        left: metapb::Region,
        right: metapb::Region,
    },
    ValidatePeer {
        region: metapb::Region,
        peer: metapb::Peer,
    },
}

impl Display for Task {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match *self {
            Task::AskSplit {
                ref region,
                ref split_key,
                ..
            } => write!(
                f,
                "ask split region {} with key {}",
                region.get_id(),
                escape(split_key)
            ),
            Task::Heartbeat {
                ref region,
                ref peer,
                ..
            } => write!(
                f,
                "heartbeat for region {:?}, leader {}",
                region,
                peer.get_id()
            ),
            Task::StoreHeartbeat { ref stats, .. } => {
                write!(f, "store heartbeat stats: {:?}", stats)
            }
            Task::ReportSplit {
                ref left,
                ref right,
            } => write!(f, "report split left {:?}, right {:?}", left, right),
            Task::ValidatePeer {
                ref region,
                ref peer,
            } => write!(f, "validate peer {:?} with region {:?}", peer, region),
        }
    }
}

pub struct Runner<T: PdClient> {
    store_id: u64,
    pd_client: Arc<T>,
    ch: SendCh<Msg>,
    db: Arc<DB>,
    is_hb_receiver_scheduled: bool,
}

impl<T: PdClient> Runner<T> {
    pub fn new(store_id: u64, pd_client: Arc<T>, ch: SendCh<Msg>, db: Arc<DB>) -> Runner<T> {
        Runner {
            store_id: store_id,
            pd_client: pd_client,
            ch: ch,
            db: db,
            is_hb_receiver_scheduled: false,
        }
    }

    fn handle_ask_split(
        &self,
        handle: &Handle,
        mut region: metapb::Region,
        split_key: Vec<u8>,
        peer: metapb::Peer,
        right_derive: bool,
        callback: Option<Callback>,
    ) {
        PD_REQ_COUNTER_VEC
            .with_label_values(&["ask split", "all"])
            .inc();

        let ch = self.ch.clone();
        let f = self.pd_client.ask_split(region.clone()).then(move |resp| {
            match resp {
                Ok(mut resp) => {
                    info!(
                        "[region {}] try to split with new region id {} for region {:?}",
                        region.get_id(),
                        resp.get_new_region_id(),
                        region
                    );
                    PD_REQ_COUNTER_VEC
                        .with_label_values(&["ask split", "success"])
                        .inc();

                    let req = new_split_region_request(
                        split_key,
                        resp.get_new_region_id(),
                        resp.take_new_peer_ids(),
                        right_derive,
                    );
                    let region_id = region.get_id();
                    let epoch = region.take_region_epoch();
                    send_admin_request(&ch, region_id, epoch, peer, req, callback)
                }
                Err(e) => {
                    debug!("[region {}] failed to ask split: {:?}", region.get_id(), e);
                }
            }
            Ok(())
        });
        handle.spawn(f)
    }

    fn handle_heartbeat(
        &self,
        handle: &Handle,
        region: metapb::Region,
        peer: metapb::Peer,
        region_stat: RegionStat,
    ) {
        PD_REQ_COUNTER_VEC
            .with_label_values(&["heartbeat", "all"])
            .inc();

        // Now we use put region protocol for heartbeat.
        let f = self.pd_client
            .region_heartbeat(region.clone(), peer.clone(), region_stat)
            .map_err(move |e| {
                debug!(
                    "[region {}] failed to send heartbeat: {:?}",
                    region.get_id(),
                    e
                );
            });
        handle.spawn(f);
    }

    fn handle_store_heartbeat(
        &self,
        handle: &Handle,
        mut stats: pdpb::StoreStats,
        store_info: StoreInfo,
    ) {
        let disk_stats = match fs2::statvfs(store_info.engine.path()) {
            Err(e) => {
                error!(
                    "get disk stat for rocksdb {} failed: {}",
                    store_info.engine.path(),
                    e
                );
                return;
            }
            Ok(stats) => stats,
        };

        let disk_cap = disk_stats.total_space();
        let capacity = if store_info.capacity == 0 || disk_cap < store_info.capacity {
            disk_cap
        } else {
            store_info.capacity
        };
        stats.set_capacity(capacity);

        let used_size = stats.get_used_size() + get_engine_used_size(store_info.engine.clone());
        stats.set_used_size(used_size);


        let mut available = if capacity > used_size {
            capacity - used_size
        } else {
            warn!("no available space");
            0
        };

        // We only care rocksdb SST file size, so we should
        // check disk available here.
        if available > disk_stats.free_space() {
            available = disk_stats.free_space();
        }

        stats.set_available(available);

        STORE_SIZE_GAUGE_VEC
            .with_label_values(&["capacity"])
            .set(capacity as f64);
        STORE_SIZE_GAUGE_VEC
            .with_label_values(&["available"])
            .set(available as f64);

        let f = self.pd_client.store_heartbeat(stats).map_err(|e| {
            error!("store heartbeat failed {:?}", e);
        });
        handle.spawn(f);
    }

    fn handle_report_split(&self, handle: &Handle, left: metapb::Region, right: metapb::Region) {
        PD_REQ_COUNTER_VEC
            .with_label_values(&["report split", "all"])
            .inc();

        let f = self.pd_client.report_split(left, right).then(move |resp| {
            match resp {
                Ok(_) => {
                    PD_REQ_COUNTER_VEC
                        .with_label_values(&["report split", "success"])
                        .inc();
                }
                Err(e) => {
                    error!("report split failed {:?}", e);
                }
            }
            Ok(())
        });
        handle.spawn(f);
    }

    fn handle_validate_peer(
        &self,
        handle: &Handle,
        local_region: metapb::Region,
        peer: metapb::Peer,
    ) {
        PD_REQ_COUNTER_VEC
            .with_label_values(&["get region", "all"])
            .inc();

        let ch = self.ch.clone();
        let f = self.pd_client.get_region_by_id(local_region.get_id()).then(move |resp| {
            match resp {
                Ok(Some(pd_region)) => {
                    PD_REQ_COUNTER_VEC.with_label_values(&["get region", "success"]).inc();
                    if is_epoch_stale(pd_region.get_region_epoch(),
                                      local_region.get_region_epoch()) {
                        // The local region epoch is fresher than region epoch in PD
                        // This means the region info in PD is not updated to the latest even
                        // after max_leader_missing_duration. Something is wrong in the system.
                        // Just add a log here for this situation.
                        error!("[region {}] {} the local region epoch: {:?} is greater the \
                                region epoch in PD: {:?}. Something is wrong!",
                               local_region.get_id(),
                               peer.get_id(),
                               local_region.get_region_epoch(),
                               pd_region.get_region_epoch());
                        PD_VALIDATE_PEER_COUNTER_VEC.with_label_values(&["region epoch error"])
                            .inc();
                        return Ok(());
                    }

                    if pd_region.get_peers().into_iter().all(|p| p != &peer) {
                        // Peer is not a member of this region anymore. Probably it's removed out.
                        // Send it a raft massage to destroy it since it's obsolete.
                        info!("[region {}] {} is not a valid member of region {:?}. To be \
                               destroyed soon.",
                              local_region.get_id(),
                              peer.get_id(),
                              pd_region);
                        PD_VALIDATE_PEER_COUNTER_VEC.with_label_values(&["peer stale"]).inc();
                        send_destroy_peer_message(ch, local_region, peer, pd_region);
                        return Ok(());
                    }
                    info!("[region {}] {} is still valid in region {:?}",
                          local_region.get_id(),
                          peer.get_id(),
                          pd_region);
                    PD_VALIDATE_PEER_COUNTER_VEC.with_label_values(&["peer valid"]).inc();
                }
                Ok(None) => {
                    // splitted region has not yet reported to pd.
                    // TODO: handle merge
                }
                Err(e) => {
                    error!("get region failed {:?}", e);
                }
            }
            Ok(())
        });
        handle.spawn(f);
    }

    fn schedule_heartbeat_receiver(&mut self, handle: &Handle) {
        let ch = self.ch.clone();
        let store_id = self.store_id;
        let f = self.pd_client
            .handle_region_heartbeat_response(self.store_id, move |mut resp| {
                PD_REQ_COUNTER_VEC
                    .with_label_values(&["heartbeat", "success"])
                    .inc();
                let region_id = resp.get_region_id();
                let epoch = resp.take_region_epoch();
                let peer = resp.take_target_peer();

                if resp.has_change_peer() {
                    PD_HEARTBEAT_COUNTER_VEC
                        .with_label_values(&["change peer"])
                        .inc();

                    let mut change_peer = resp.take_change_peer();
                    info!(
                        "[region {}] try to change peer {:?} {:?}",
                        region_id,
                        change_peer.get_change_type(),
                        change_peer.get_peer()
                    );
                    let req = new_change_peer_request(
                        change_peer.get_change_type().into(),
                        change_peer.take_peer(),
                    );
                    send_admin_request(&ch, region_id, epoch, peer, req, None);
                } else if resp.has_transfer_leader() {
                    PD_HEARTBEAT_COUNTER_VEC
                        .with_label_values(&["transfer leader"])
                        .inc();

                    let mut transfer_leader = resp.take_transfer_leader();
                    info!(
                        "[region {}] try to transfer leader from {:?} to {:?}",
                        region_id,
                        peer,
                        transfer_leader.get_peer()
                    );
                    let req = new_transfer_leader_request(transfer_leader.take_peer());
                    send_admin_request(&ch, region_id, epoch, peer, req, None)
                }
            })
            .map_err(|e| panic!("unexpected error: {:?}", e))
            .map(move |_| {
                info!(
                    "[store {}] region heartbeat response handler exit.",
                    store_id
                )
            });
        handle.spawn(f);
        self.is_hb_receiver_scheduled = true;
    }
}

impl<T: PdClient> Runnable<Task> for Runner<T> {
    fn run(&mut self, task: Task, handle: &Handle) {
        debug!("executing task {}", task);

        if !self.is_hb_receiver_scheduled {
            self.schedule_heartbeat_receiver(handle);
        }

        match task {
            Task::AskSplit {
                region,
                split_key,
                peer,
                right_derive,
                callback,
            } => self.handle_ask_split(handle, region, split_key, peer, right_derive, callback),
            Task::Heartbeat {
                region,
                peer,
                down_peers,
                pending_peers,
                written_bytes,
                written_keys,
                read_bytes,
                read_keys,
            } => {
                let approximate_size = get_region_approximate_size(&self.db, &region).unwrap_or(0);
                self.handle_heartbeat(
                    handle,
                    region,
                    peer,
                    RegionStat::new(
                        down_peers,
                        pending_peers,
                        written_bytes,
                        written_keys,
                        read_bytes,
                        read_keys,
                        approximate_size,
                    ),
                )
            }
            Task::StoreHeartbeat { stats, store_info } => {
                self.handle_store_heartbeat(handle, stats, store_info)
            }
            Task::ReportSplit { left, right } => self.handle_report_split(handle, left, right),
            Task::ValidatePeer { region, peer } => self.handle_validate_peer(handle, region, peer),
        };
    }
}

fn new_change_peer_request(change_type: ConfChangeType, peer: metapb::Peer) -> AdminRequest {
    let mut req = AdminRequest::new();
    req.set_cmd_type(AdminCmdType::ChangePeer);
    req.mut_change_peer().set_change_type(change_type.into());
    req.mut_change_peer().set_peer(peer);
    req
}

fn new_split_region_request(
    split_key: Vec<u8>,
    new_region_id: u64,
    peer_ids: Vec<u64>,
    right_derive: bool,
) -> AdminRequest {
    let mut req = AdminRequest::new();
    req.set_cmd_type(AdminCmdType::Split);
    req.mut_split().set_split_key(split_key);
    req.mut_split().set_new_region_id(new_region_id);
    req.mut_split().set_new_peer_ids(peer_ids);
    req.mut_split().set_right_derive(right_derive);
    req
}

fn new_transfer_leader_request(peer: metapb::Peer) -> AdminRequest {
    let mut req = AdminRequest::new();
    req.set_cmd_type(AdminCmdType::TransferLeader);
    req.mut_transfer_leader().set_peer(peer);
    req
}

fn send_admin_request(
    ch: &SendCh<Msg>,
    region_id: u64,
    epoch: metapb::RegionEpoch,
    peer: metapb::Peer,
    request: AdminRequest,
    callback: Option<Callback>,
) {
    let cmd_type = request.get_cmd_type();

    let mut req = RaftCmdRequest::new();
    req.mut_header().set_region_id(region_id);
    req.mut_header().set_region_epoch(epoch);
    req.mut_header().set_peer(peer);

    req.set_admin_request(request);

    if let Err(e) = ch.try_send(Msg::new_raft_cmd(
        req,
        callback.unwrap_or_else(|| Box::new(|_| {})),
    )) {
        error!(
            "[region {}] send {:?} request err {:?}",
            region_id,
            cmd_type,
            e
        );
    }
}

// send a raft message to destroy the specified stale peer
fn send_destroy_peer_message(
    ch: SendCh<Msg>,
    local_region: metapb::Region,
    peer: metapb::Peer,
    pd_region: metapb::Region,
) {
    let mut message = RaftMessage::new();
    message.set_region_id(local_region.get_id());
    message.set_from_peer(peer.clone());
    message.set_to_peer(peer.clone());
    message.set_region_epoch(pd_region.get_region_epoch().clone());
    message.set_is_tombstone(true);
    if let Err(e) = ch.try_send(Msg::RaftMessage(message)) {
        error!(
            "send gc peer request to region {} err {:?}",
            local_region.get_id(),
            e
        )
    }
}
