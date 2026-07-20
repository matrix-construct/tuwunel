mod clear;
mod compact;
mod count;
mod del;
mod flush;
mod get;
mod iter;
mod keys;
mod keys_sizes;
mod keys_total;
mod maps;
mod put;
mod sequence;
mod vals_sizes;
mod vals_total;

use std::{fmt::Write, sync::Arc};

use clap::Subcommand;
use tuwunel_core::{Result, err, expected, itertools::Itertools, utils::math::Expected};
use tuwunel_database::Map;
use tuwunel_service::Services;

use crate::admin_command_dispatch;

#[admin_command_dispatch(handler_prefix = "raw")]
#[derive(Debug, Subcommand)]
/// Query tables from database
pub(crate) enum RawCommand {
	/// - List database maps
	Maps,

	/// - Current rocksdb sequence number.
	Sequence,

	/// - Raw database query
	Get {
		/// Map name
		map: String,

		/// Key
		key: String,

		/// Encode as base64
		#[arg(long, short)]
		base64: bool,
	},

	/// - Raw database keys iteration
	Keys {
		/// Map name
		map: String,

		/// Key prefix
		prefix: Option<String>,

		/// Limit
		#[arg(short, long)]
		limit: Option<usize>,

		/// Lower bound
		#[arg(short, long)]
		from: Option<String>,

		/// Reverse iteration order
		#[arg(short, long, default_value("false"))]
		backwards: bool,
	},

	/// - Raw database items iteration
	Iter {
		/// Map name
		map: String,

		/// Key prefix
		prefix: Option<String>,

		/// Limit
		#[arg(short, long)]
		limit: Option<usize>,

		/// Lower bound
		#[arg(short, long)]
		from: Option<String>,

		/// Reverse iteration order
		#[arg(short, long, default_value("false"))]
		backwards: bool,
	},

	/// - Raw database key size breakdown
	KeysSizes {
		/// Map name
		map: Option<String>,

		/// Key prefix
		prefix: Option<String>,
	},

	/// - Raw database keys total bytes
	KeysTotal {
		/// Map name
		map: Option<String>,

		/// Key prefix
		prefix: Option<String>,
	},

	/// - Raw database values size breakdown
	ValsSizes {
		/// Map name
		map: Option<String>,

		/// Key prefix
		prefix: Option<String>,
	},

	/// - Raw database values total bytes
	ValsTotal {
		/// Map name
		map: Option<String>,

		/// Key prefix
		prefix: Option<String>,
	},

	/// - Raw database record count
	Count {
		/// Map name
		map: Option<String>,

		/// Key prefix
		prefix: Option<String>,
	},

	/// - Raw database put
	Put {
		/// Map name
		map: String,

		/// Key
		key: String,

		/// Value
		value: String,
	},

	/// - Raw database delete (for string keys) DANGER!!!
	Del {
		/// Map name
		map: String,

		/// Key
		key: String,
	},

	/// - Clear database table DANGER!!!
	Clear {
		/// Map name
		map: String,

		/// Confirm
		#[arg(long)]
		confirm: bool,
	},

	/// - Compact database DANGER!!!
	Compact {
		#[arg(short, long, alias("column"))]
		maps: Option<Vec<String>>,

		#[arg(long)]
		start: Option<String>,

		#[arg(long)]
		stop: Option<String>,

		#[arg(long)]
		from: Option<usize>,

		#[arg(long)]
		into: Option<usize>,

		/// There is one compaction job per column; then this controls how many
		/// columns are compacted in parallel. If zero, one compaction job is
		/// still run at a time here, but in exclusive-mode blocking any other
		/// automatic compaction jobs until complete.
		#[arg(long)]
		parallelism: Option<usize>,

		#[arg(long, default_value("false"))]
		exhaustive: bool,
	},

	/// - Flush RocksDB memtables to SST files (LSM flush, not fsync/fflush)
	Flush,
}

fn with_map_or(map: Option<&str>, services: &Services) -> Result<Vec<Arc<Map>>> {
	with_maps_or(
		map.map(|map| [map])
			.as_ref()
			.map(<[&str; 1]>::as_slice),
		services,
	)
}

fn with_maps_or<S: AsRef<str>>(maps: Option<&[S]>, services: &Services) -> Result<Vec<Arc<Map>>> {
	Ok(if let Some(maps) = maps {
		maps.iter()
			.map(|map| {
				let map = map.as_ref();
				services
					.db
					.get(map)
					.cloned()
					.map_err(|_| err!("map {map} not found"))
			})
			.try_collect()?
	} else {
		services.db.iter().map(|x| x.1.clone()).collect()
	})
}

fn from_hex(byte: u8) -> Option<u8> {
	match byte {
		| 0x30..=0x39 => Some(expected!(byte - 0x30)),
		| 0x41..=0x46 => Some(expected!((byte - 0x41) + 10)),
		| 0x61..=0x66 => Some(expected!((byte - 0x61) + 10)),
		| _ => None,
	}
}

fn decode(data: &str) -> Vec<u8> {
	let mut res = Vec::with_capacity(data.len());

	for byte in data.bytes() {
		res.push(byte);

		let length = res.len();

		if length >= 4
			&& let Some(slice) = res.get(expected!(length - 4)..length)
			&& slice.starts_with(b"\\x")
			&& let Some(a) = from_hex(slice[2])
			&& let Some(b) = from_hex(slice[3])
		{
			res.truncate(expected!(length - 4));

			let byte = (a << 4) | b;
			res.push(byte);
		}
	}

	res
}

#[expect(clippy::as_conversions)]
fn encode(data: &[u8]) -> String {
	let mut res = String::with_capacity(data.len().expected_mul(4));

	for byte in data {
		if *byte < 0x20 || *byte > 0x7E {
			_ = write!(res, "\\x{byte:02x}");
		} else {
			res.push(*byte as char);
		}
	}

	res
}
