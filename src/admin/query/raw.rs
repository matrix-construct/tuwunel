use std::{borrow::Cow, collections::BTreeMap, ops::Deref, sync::Arc};

use base64::prelude::*;
use clap::Subcommand;
use futures::{FutureExt, Stream, StreamExt, TryStreamExt};
use tokio::time::Instant;
use tuwunel_core::{
	Err, Result, apply, at, is_zero,
	utils::{
		stream::{IterStream, ReadyExt, TryIgnore, TryParallelExt},
		string::EMPTY,
	},
};
use tuwunel_database::Map;
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

	/// - Raw database items iteration
	Iter {
		/// Map name
		map: String,

		/// Key prefix
		prefix: Option<String>,
	},

	/// - Raw database keys iteration
	KeysFrom {
		/// Map name
		map: String,

		/// Lower-bound
		start: String,

		/// Limit
		#[arg(short, long)]
		limit: Option<usize>,
	},

	/// - Raw database items iteration
	IterFrom {
		/// Map name
		map: String,

		/// Lower-bound
		start: String,

		/// Limit
		#[arg(short, long)]
		limit: Option<usize>,
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
		map: Option<Vec<String>>,

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
	map: Option<Vec<String>>,
	start: Option<String>,
	stop: Option<String>,
	from: Option<usize>,
	into: Option<usize>,
	parallelism: Option<usize>,
	exhaustive: bool,
) -> Result {
	use tuwunel_database::compact::Options;

	let default_all_maps: Option<_> = map.is_none().then(|| {
		self.services
			.db
			.keys()
			.map(Deref::deref)
			.map(ToOwned::to_owned)
	});

	let maps: Vec<_> = map
		.unwrap_or_default()
		.into_iter()
		.chain(default_all_maps.into_iter().flatten())
		.map(|map| self.services.db.get(&map))
		.filter_map(Result::ok)
		.cloned()
		.collect();

	if maps.is_empty() {
		return Err!("--map argument invalid. not found in database");
	}

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
	let count = with_maps_or(map.as_deref(), self.services)
		.then(|map| map.raw_count_prefix(&prefix))
		.ready_fold(0_usize, usize::saturating_add)
		.await;

	let query_time = timer.elapsed();
	self.write_str(&format!("Query completed in {query_time:?}:\n\n```rs\n{count:#?}\n```"))
		.await
}

#[admin_command]
pub(super) async fn raw_keys(&self, map: String, prefix: Option<String>) -> Result {
	writeln!(self, "```").boxed().await?;

	let map = self.services.db.get(map.as_str())?;
	let timer = Instant::now();
	prefix
		.as_deref()
		.map_or_else(|| map.raw_keys().boxed(), |prefix| map.raw_keys_prefix(prefix).boxed())
		.map_ok(String::from_utf8_lossy)
		.try_for_each(|str| writeln!(self, "{str:?}"))
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
	let result = with_maps_or(map.as_deref(), self.services)
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
	let result = with_maps_or(map.as_deref(), self.services)
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
	let result = with_maps_or(map.as_deref(), self.services)
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
	let result = with_maps_or(map.as_deref(), self.services)
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
pub(super) async fn raw_iter(&self, map: String, prefix: Option<String>) -> Result {
	writeln!(self, "```").await?;

	let map = self.services.db.get(&map)?;
	let timer = Instant::now();
	prefix
		.as_deref()
		.map_or_else(|| map.raw_stream().boxed(), |prefix| map.raw_stream_prefix(prefix).boxed())
		.map_ok(apply!(2, String::from_utf8_lossy))
		.map_ok(apply!(2, Cow::into_owned))
		.try_for_each(|keyval| writeln!(self, "{keyval:?}"))
		.boxed()
		.await?;

	let query_time = timer.elapsed();
	self.write_str(&format!("\n```\n\nQuery completed in {query_time:?}"))
		.await
}

#[admin_command]
pub(super) async fn raw_keys_from(
	&self,
	map: String,
	start: String,
	limit: Option<usize>,
) -> Result {
	writeln!(self, "```").await?;

	let map = self.services.db.get(&map)?;
	let timer = Instant::now();
	map.raw_keys_from(&start)
		.map_ok(String::from_utf8_lossy)
		.take(limit.unwrap_or(usize::MAX))
		.try_for_each(|str| writeln!(self, "{str:?}"))
		.boxed()
		.await?;

	let query_time = timer.elapsed();
	self.write_str(&format!("\n```\n\nQuery completed in {query_time:?}"))
		.await
}

#[admin_command]
pub(super) async fn raw_iter_from(
	&self,
	map: String,
	start: String,
	limit: Option<usize>,
) -> Result {
	let map = self.services.db.get(&map)?;
	let timer = Instant::now();
	let result = map
		.raw_stream_from(&start)
		.map_ok(apply!(2, String::from_utf8_lossy))
		.map_ok(apply!(2, Cow::into_owned))
		.take(limit.unwrap_or(usize::MAX))
		.try_collect::<Vec<(String, String)>>()
		.await?;

	let query_time = timer.elapsed();
	self.write_str(&format!("Query completed in {query_time:?}:\n\n```rs\n{result:#?}\n```"))
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
		String::from_utf8_lossy(&handle).to_string()
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

fn with_maps_or<'a>(
	map: Option<&'a str>,
	services: &'a Services,
) -> impl Stream<Item = &'a Arc<Map>> + Send + 'a {
	let default_all_maps = map
		.is_none()
		.then(|| services.db.keys().map(Deref::deref))
		.into_iter()
		.flatten();

	map.into_iter()
		.chain(default_all_maps)
		.map(|map| services.db.get(map))
		.filter_map(Result::ok)
		.stream()
}
