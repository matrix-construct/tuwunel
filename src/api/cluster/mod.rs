//! Primary-side HTTP handlers for WAL-based RocksDB replication.
//!
//! Endpoints (all protected by `check_replication_token` middleware):
//! - `GET  /_tuwunel/cluster/status` — current sequence number + role
//! - `GET  /_tuwunel/cluster/sync?since=N` — streaming WAL frame feed
//! - `GET  /_tuwunel/cluster/checkpoint` — full database checkpoint as tar
//! - `POST /_tuwunel/cluster/promote` — promote secondary to primary
//! - `POST /_tuwunel/cluster/demote` — demote primary back to secondary

pub(super) mod checkpoint;
pub(super) mod demote;
pub(super) mod promote;
pub(super) mod status;
pub(super) mod sync;

pub(super) use checkpoint::*;
pub(super) use demote::*;
pub(super) use promote::*;
pub(super) use status::*;
pub(super) use sync::*;
