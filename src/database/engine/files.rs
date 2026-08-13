use rocksdb::LiveFile as SstFile;
use tuwunel_core::{Result, implement};

use super::Engine;
use crate::util::map_err;

/// Lists the live SST files belonging to this database.
///
/// RocksDB produces the inventory before the iterator is returned. Each yielded
/// entry describes one table file from that in-memory inventory.
#[implement(Engine)]
pub fn file_list(&self) -> impl Iterator<Item = Result<SstFile>> + Send + use<> {
	self.db
		.live_files()
		.map_err(map_err)
		.into_iter()
		.flat_map(Vec::into_iter)
		.map(Ok)
}
