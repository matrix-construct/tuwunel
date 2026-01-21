use std::{collections::BTreeMap, fmt::Write, sync::Arc};

use base64::prelude::*;
use clap::Subcommand;
use futures::{FutureExt, StreamExt, TryStreamExt};
use tokio::time::Instant;
use tuwunel_core::{
	Err, Result, apply, at, err, is_zero,
	itertools::Itertools,
	utils::{
		TryReadyExt,
		math::Expected,
		stream::{IterStream, ReadyExt, TryIgnore, TryParallelExt},
		string::EMPTY,
	},
};
use tuwunel_database::{KeyVal, Map};
use tuwunel_service::Services;

use crate::{admin_command, admin_command_dispatch};

#[admin_command_dispatch(handler_prefix = "raw")]
#[derive(Debug, Subcommand)]
/// Query tables from database
pub(crate) enum RawCommand {
	/// - List database maps
	Maps,

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
}

#[admin_command]
pub(super) async fn raw_compact(
	&self,
	maps: Option<Vec<String>>,
	start: Option<String>,
	stop: Option<String>,
	from: Option<usize>,
	into: Option<usize>,
	parallelism: Option<usize>,
	exhaustive: bool,
) -> Result {
	use tuwunel_database::compact::Options;

	let maps = with_maps_or(maps.as_deref(), self.services)?;

	let range = (
		start
			.as_ref()
			.map(String::as_bytes)
			.map(Into::into),
		stop.as_ref()
			.map(String::as_bytes)
			.map(Into::into),
	);

	let options = Options {
		range,
		level: (from, into),
		exclusive: parallelism.is_some_and(is_zero!()),
		exhaustive,
	};

	let runtime = self.services.server.runtime().clone();
	let parallelism = parallelism.unwrap_or(1);
	let results = maps
		.into_iter()
		.try_stream()
		.paralleln_and_then(runtime, parallelism, move |map| {
			map.compact_blocking(options.clone())?;
			Ok(map.name().to_owned())
		})
		.collect::<Vec<_>>();

	let timer = Instant::now();
	let results = results.await;
	let query_time = timer.elapsed();
	self.write_str(&format!("Jobs completed in {query_time:?}:\n\n```rs\n{results:#?}\n```"))
		.await
}

#[admin_command]
pub(super) async fn raw_count(&self, map: Option<String>, prefix: Option<String>) -> Result {
	let prefix = prefix.as_deref().unwrap_or(EMPTY);

	let timer = Instant::now();
	let count = with_map_or(map.as_deref(), self.services)?
		.iter()
		.stream()
		.then(|map| map.raw_count_prefix(&prefix))
		.ready_fold(0_usize, usize::saturating_add)
		.await;

	let query_time = timer.elapsed();
	self.write_str(&format!("Query completed in {query_time:?}:\n\n```rs\n{count:#?}\n```"))
		.await
}

#[admin_command]
pub(super) async fn raw_keys(
	&self,
	map: String,
	prefix: Option<String>,
	limit: Option<usize>,
	from: Option<String>,
	backwards: bool,
) -> Result {
	writeln!(self, "```").boxed().await?;

	let map = self.services.db.get(map.as_str())?;
	let timer = Instant::now();

	let stream = match from.as_ref().or(prefix.as_ref()) {
		| Some(from) =>
			if !backwards {
				map.raw_keys_from(from).boxed()
			} else {
				map.rev_raw_keys_from(from).boxed()
			},
		| None =>
			if !backwards {
				map.raw_keys().boxed()
			} else {
				map.rev_raw_keys().boxed()
			},
	};

	let prefix = prefix.as_ref().map(String::as_bytes);

	stream
		.ready_try_take_while(|k| {
			Ok(prefix
				.map(|prefix| k.starts_with(prefix))
				.unwrap_or(true))
		})
		.take(limit.unwrap_or(usize::MAX))
		.map_ok(encode)
		.try_for_each(|str| writeln!(self, "{str}"))
		.boxed()
		.await?;

	let query_time = timer.elapsed();
	self.write_str(&format!("\n```\n\nQuery completed in {query_time:?}"))
		.await
}

#[admin_command]
pub(super) async fn raw_keys_sizes(&self, map: Option<String>, prefix: Option<String>) -> Result {
	let prefix = prefix.as_deref().unwrap_or(EMPTY);

	let timer = Instant::now();
	let result = with_map_or(map.as_deref(), self.services)?
		.iter()
		.stream()
		.map(|map| map.raw_keys_prefix(&prefix))
		.flatten()
		.ignore_err()
		.map(<[u8]>::len)
		.ready_fold_default(|mut map: BTreeMap<_, usize>, len| {
			let entry = map.entry(len).or_default();
			*entry = entry.saturating_add(1);
			map
		})
		.await;

	let query_time = timer.elapsed();
	self.write_str(&format!("```\n{result:#?}\n```\n\nQuery completed in {query_time:?}"))
		.await
}

#[admin_command]
pub(super) async fn raw_keys_total(&self, map: Option<String>, prefix: Option<String>) -> Result {
	let prefix = prefix.as_deref().unwrap_or(EMPTY);

	let timer = Instant::now();
	let result = with_map_or(map.as_deref(), self.services)?
		.iter()
		.stream()
		.map(|map| map.raw_keys_prefix(&prefix))
		.flatten()
		.ignore_err()
		.map(<[u8]>::len)
		.ready_fold_default(|acc: usize, len| acc.saturating_add(len))
		.await;

	let query_time = timer.elapsed();
	self.write_str(&format!("```\n{result:#?}\n\n```\n\nQuery completed in {query_time:?}"))
		.await
}

#[admin_command]
pub(super) async fn raw_vals_sizes(&self, map: Option<String>, prefix: Option<String>) -> Result {
	let prefix = prefix.as_deref().unwrap_or(EMPTY);

	let timer = Instant::now();
	let result = with_map_or(map.as_deref(), self.services)?
		.iter()
		.stream()
		.map(|map| map.raw_stream_prefix(&prefix))
		.flatten()
		.ignore_err()
		.map(at!(1))
		.map(<[u8]>::len)
		.ready_fold_default(|mut map: BTreeMap<_, usize>, len| {
			let entry = map.entry(len).or_default();
			*entry = entry.saturating_add(1);
			map
		})
		.await;

	let query_time = timer.elapsed();
	self.write_str(&format!("```\n{result:#?}\n```\n\nQuery completed in {query_time:?}"))
		.await
}

#[admin_command]
pub(super) async fn raw_vals_total(&self, map: Option<String>, prefix: Option<String>) -> Result {
	let prefix = prefix.as_deref().unwrap_or(EMPTY);

	let timer = Instant::now();
	let result = with_map_or(map.as_deref(), self.services)?
		.iter()
		.stream()
		.map(|map| map.raw_stream_prefix(&prefix))
		.flatten()
		.ignore_err()
		.map(at!(1))
		.map(<[u8]>::len)
		.ready_fold_default(|acc: usize, len| acc.saturating_add(len))
		.await;

	let query_time = timer.elapsed();
	self.write_str(&format!("```\n{result:#?}\n\n```\n\nQuery completed in {query_time:?}"))
		.await
}

#[admin_command]
pub(super) async fn raw_iter(
	&self,
	map: String,
	prefix: Option<String>,
	limit: Option<usize>,
	from: Option<String>,
	backwards: bool,
) -> Result {
	writeln!(self, "```").await?;

	let map = self.services.db.get(&map)?;
	let timer = Instant::now();
	let stream = match from.as_ref().or(prefix.as_ref()) {
		| Some(from) =>
			if !backwards {
				map.raw_stream_from(from).boxed()
			} else {
				map.rev_raw_stream_from(from).boxed()
			},
		| None =>
			if !backwards {
				map.raw_stream().boxed()
			} else {
				map.rev_raw_stream().boxed()
			},
	};

	let prefix = prefix.as_ref().map(String::as_bytes);

	stream
		.ready_try_take_while(|(k, _): &KeyVal<'_>| {
			Ok(prefix
				.map(|prefix| k.starts_with(prefix))
				.unwrap_or(true))
		})
		.take(limit.unwrap_or(usize::MAX))
		.map_ok(apply!(2, encode))
		.try_for_each(|(key, val)| writeln!(self, "{{{key} => {val}}}"))
		.boxed()
		.await?;

	let query_time = timer.elapsed();
	self.write_str(&format!("\n```\n\nQuery completed in {query_time:?}"))
		.await
}

#[admin_command]
pub(super) async fn raw_del(&self, map: String, key: String) -> Result {
	let map = self.services.db.get(&map)?;
	let timer = Instant::now();
	map.remove(&key);

	let query_time = timer.elapsed();
	self.write_str(&format!("Operation completed in {query_time:?}"))
		.await
}

#[admin_command]
pub(super) async fn raw_clear(&self, map: String, confirm: bool) -> Result {
	let map = self.services.db.get(&map)?;

	if !confirm {
		return Err!("Are you really sure you want to clear all data? Add the --confirm option.");
	}

	let timer = Instant::now();
	let cork = self.services.db.cork();
	map.raw_keys()
		.ignore_err()
		.ready_for_each(|key| map.remove(&key))
		.boxed()
		.await;

	drop(cork);
	let query_time = timer.elapsed();
	self.write_str(&format!("Operation completed in {query_time:?}"))
		.await
}

#[admin_command]
pub(super) async fn raw_get(&self, map: String, key: String, base64: bool) -> Result {
	let map = self.services.db.get(&map)?;
	let timer = Instant::now();
	let handle = map.get(&key).await?;

	let query_time = timer.elapsed();

	let result = if base64 {
		BASE64_STANDARD.encode(&handle)
	} else {
		encode(&handle)
	};

	self.write_str(&format!("Query completed in {query_time:?}:\n\n```rs\n{result:?}\n```"))
		.await
}

#[admin_command]
pub(super) async fn raw_maps(&self) -> Result {
	let list: Vec<_> = self
		.services
		.db
		.iter()
		.map(at!(0))
		.copied()
		.collect();

	self.write_str(&format!("{list:#?}")).await
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

#[expect(clippy::as_conversions)]
fn encode(data: &[u8]) -> String {
	let mut res = String::with_capacity(data.len().expected_mul(4));

	for byte in data {
		if *byte < 0x20 || *byte > 0x7E {
			let _ = write!(res, "\\x{byte:02x}");
		} else {
			res.push(*byte as char);
		}
	}

	res
}
