#![feature(backtrace)]

#[cfg(test)]
mod test;

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt::Debug;
use std::io::Cursor;
use std::ops::RangeBounds;
use std::sync::Arc;
use std::sync::Mutex;

use anyerror::AnyError;
use openraft::async_trait::async_trait;
use openraft::raft::Entry;
use openraft::raft::EntryPayload;
use openraft::storage::LogState;
use openraft::storage::Snapshot;
use openraft::AppData;
use openraft::AppDataResponse;
use openraft::EffectiveMembership;
use openraft::ErrorSubject;
use openraft::ErrorVerb;
use openraft::LogId;
use openraft::RaftStorage;
use openraft::RaftStorageDebug;
use openraft::SnapshotMeta;
use openraft::StateMachineChanges;
use openraft::StorageError;
use openraft::StorageIOError;
use openraft::Vote;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::RwLock;

/// The application data request type which the `MemStore` works with.
///
/// Conceptually, for demo purposes, this represents an update to a client's status info,
/// returning the previously recorded status.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ClientRequest {
    /// The ID of the client which has sent the request.
    pub client: String,

    /// The serial number of this request.
    pub serial: u64,

    /// A string describing the status of the client. For a real application, this should probably
    /// be an enum representing all of the various types of requests / operations which a client
    /// can perform.
    pub status: String,
}

impl AppData for ClientRequest {}

/// The application data response type which the `MemStore` works with.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ClientResponse(Option<String>);

impl AppDataResponse for ClientResponse {}

/// The application snapshot type which the `MemStore` works with.
#[derive(Debug)]
pub struct MemStoreSnapshot {
    pub meta: SnapshotMeta,

    /// The data of the state machine at the time of this snapshot.
    pub data: Vec<u8>,
}

/// The state machine of the `MemStore`.
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct MemStoreStateMachine {
    pub last_applied_log: Option<LogId>,

    pub last_membership: Option<EffectiveMembership>,

    /// A mapping of client IDs to their state info.
    pub client_serial_responses: HashMap<String, (u64, Option<String>)>,
    /// The current status of a client by ID.
    pub client_status: HashMap<String, String>,
}

/// An in-memory storage system implementing the `RaftStorage` trait.
pub struct MemStore {
    last_purged_log_id: RwLock<Option<LogId>>,

    /// The Raft log.
    log: RwLock<BTreeMap<u64, Entry<ClientRequest>>>,

    /// The Raft state machine.
    sm: RwLock<MemStoreStateMachine>,

    /// The current hard state.
    vote: RwLock<Option<Vote>>,

    snapshot_idx: Arc<Mutex<u64>>,

    /// The current snapshot.
    current_snapshot: RwLock<Option<MemStoreSnapshot>>,
}

impl MemStore {
    /// Create a new `MemStore` instance.
    pub async fn new() -> Self {
        let log = RwLock::new(BTreeMap::new());
        let sm = RwLock::new(MemStoreStateMachine::default());
        let current_snapshot = RwLock::new(None);

        Self {
            last_purged_log_id: RwLock::new(None),
            log,
            sm,
            vote: RwLock::new(None),
            snapshot_idx: Arc::new(Mutex::new(0)),
            current_snapshot,
        }
    }
}

#[async_trait]
impl RaftStorageDebug<MemStoreStateMachine> for MemStore {
    /// Get a handle to the state machine for testing purposes.
    async fn get_state_machine(&self) -> MemStoreStateMachine {
        self.sm.write().await.clone()
    }
}

#[async_trait]
impl RaftStorage<ClientRequest, ClientResponse> for MemStore {
    type SnapshotData = Cursor<Vec<u8>>;

    #[tracing::instrument(level = "trace", skip(self))]
    async fn save_vote(&self, vote: &Vote) -> Result<(), StorageError> {
        tracing::debug!(?vote, "save_vote");
        let mut h = self.vote.write().await;

        *h = Some(*vote);
        Ok(())
    }

    async fn read_vote(&self) -> Result<Option<Vote>, StorageError> {
        Ok(*self.vote.read().await)
    }

    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send + Sync>(
        &self,
        range: RB,
    ) -> Result<Vec<Entry<ClientRequest>>, StorageError> {
        let res = {
            let log = self.log.read().await;
            log.range(range.clone()).map(|(_, val)| val.clone()).collect::<Vec<_>>()
        };

        Ok(res)
    }

    async fn get_log_state(&self) -> Result<LogState, StorageError> {
        let log = self.log.read().await;
        let last = log.iter().rev().next().map(|(_, ent)| ent.log_id);

        let last_deleted = *self.last_purged_log_id.read().await;

        let last = match last {
            None => last_deleted,
            Some(x) => Some(x),
        };

        Ok(LogState {
            last_purged_log_id: last_deleted,
            last_log_id: last,
        })
    }

    async fn last_applied_state(&self) -> Result<(Option<LogId>, Option<EffectiveMembership>), StorageError> {
        let sm = self.sm.read().await;
        Ok((sm.last_applied_log, sm.last_membership.clone()))
    }

    #[tracing::instrument(level = "debug", skip(self))]
    async fn delete_conflict_logs_since(&self, log_id: LogId) -> Result<(), StorageError> {
        tracing::debug!("delete_log: [{:?}, +oo)", log_id);

        {
            let mut log = self.log.write().await;

            let keys = log.range(log_id.index..).map(|(k, _v)| *k).collect::<Vec<_>>();
            for key in keys {
                log.remove(&key);
            }
        }

        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self))]
    async fn purge_logs_upto(&self, log_id: LogId) -> Result<(), StorageError> {
        tracing::debug!("delete_log: [{:?}, +oo)", log_id);

        {
            let mut ld = self.last_purged_log_id.write().await;
            assert!(*ld <= Some(log_id));
            *ld = Some(log_id);
        }

        {
            let mut log = self.log.write().await;

            let keys = log.range(..=log_id.index).map(|(k, _v)| *k).collect::<Vec<_>>();
            for key in keys {
                log.remove(&key);
            }
        }

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip(self, entries))]
    async fn append_to_log(&self, entries: &[&Entry<ClientRequest>]) -> Result<(), StorageError> {
        let mut log = self.log.write().await;
        for entry in entries {
            log.insert(entry.log_id.index, (*entry).clone());
        }
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip(self, entries))]
    async fn apply_to_state_machine(
        &self,
        entries: &[&Entry<ClientRequest>],
    ) -> Result<Vec<ClientResponse>, StorageError> {
        let mut res = Vec::with_capacity(entries.len());

        let mut sm = self.sm.write().await;

        for entry in entries {
            tracing::debug!(%entry.log_id, "replicate to sm");

            sm.last_applied_log = Some(entry.log_id);

            match entry.payload {
                EntryPayload::Blank => res.push(ClientResponse(None)),
                EntryPayload::Normal(ref data) => {
                    if let Some((serial, r)) = sm.client_serial_responses.get(&data.client) {
                        if serial == &data.serial {
                            res.push(ClientResponse(r.clone()));
                            continue;
                        }
                    }
                    let previous = sm.client_status.insert(data.client.clone(), data.status.clone());
                    sm.client_serial_responses.insert(data.client.clone(), (data.serial, previous.clone()));
                    res.push(ClientResponse(previous));
                }
                EntryPayload::Membership(ref mem) => {
                    sm.last_membership = Some(EffectiveMembership {
                        log_id: entry.log_id,
                        membership: mem.clone(),
                    });
                    res.push(ClientResponse(None))
                }
            };
        }
        Ok(res)
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn build_snapshot(&self) -> Result<Snapshot<Self::SnapshotData>, StorageError> {
        let (data, last_applied_log);

        {
            // Serialize the data of the state machine.
            let sm = self.sm.read().await;
            data = serde_json::to_vec(&*sm)
                .map_err(|e| StorageIOError::new(ErrorSubject::StateMachine, ErrorVerb::Read, AnyError::new(&e)))?;

            last_applied_log = sm.last_applied_log;
        }

        let last_applied_log = match last_applied_log {
            None => {
                panic!("can not compact empty state machine");
            }
            Some(x) => x,
        };

        let snapshot_size = data.len();

        let snapshot_idx = {
            let mut l = self.snapshot_idx.lock().unwrap();
            *l += 1;
            *l
        };

        let snapshot_id = format!(
            "{}-{}-{}",
            last_applied_log.leader_id, last_applied_log.index, snapshot_idx
        );

        let meta = SnapshotMeta {
            last_log_id: last_applied_log,
            snapshot_id,
        };

        let snapshot = MemStoreSnapshot {
            meta: meta.clone(),
            data: data.clone(),
        };

        {
            let mut current_snapshot = self.current_snapshot.write().await;
            *current_snapshot = Some(snapshot);
        }

        tracing::info!(snapshot_size, "log compaction complete");

        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        })
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn begin_receiving_snapshot(&self) -> Result<Box<Self::SnapshotData>, StorageError> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    #[tracing::instrument(level = "trace", skip(self, snapshot))]
    async fn install_snapshot(
        &self,
        meta: &SnapshotMeta,
        snapshot: Box<Self::SnapshotData>,
    ) -> Result<StateMachineChanges, StorageError> {
        tracing::info!(
            { snapshot_size = snapshot.get_ref().len() },
            "decoding snapshot for installation"
        );

        let new_snapshot = MemStoreSnapshot {
            meta: meta.clone(),
            data: snapshot.into_inner(),
        };

        {
            let t = &new_snapshot.data;
            let y = std::str::from_utf8(t).unwrap();
            tracing::debug!("SNAP META:{:?}", meta);
            tracing::debug!("JSON SNAP DATA:{}", y);
        }

        // Update the state machine.
        {
            let new_sm: MemStoreStateMachine = serde_json::from_slice(&new_snapshot.data).map_err(|e| {
                StorageIOError::new(
                    ErrorSubject::Snapshot(new_snapshot.meta.clone()),
                    ErrorVerb::Read,
                    AnyError::new(&e),
                )
            })?;
            let mut sm = self.sm.write().await;
            *sm = new_sm;
        }

        // Update current snapshot.
        let mut current_snapshot = self.current_snapshot.write().await;
        *current_snapshot = Some(new_snapshot);
        Ok(StateMachineChanges {
            last_applied: meta.last_log_id,
            is_snapshot: true,
        })
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn get_current_snapshot(&self) -> Result<Option<Snapshot<Self::SnapshotData>>, StorageError> {
        match &*self.current_snapshot.read().await {
            Some(snapshot) => {
                let data = snapshot.data.clone();
                Ok(Some(Snapshot {
                    meta: snapshot.meta.clone(),
                    snapshot: Box::new(Cursor::new(data)),
                }))
            }
            None => Ok(None),
        }
    }
}
