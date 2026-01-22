use rocksdb::{
	BlockBasedIndexType, BlockBasedOptions, BlockBasedPinningTier, Cache,
	DBCompressionType as CompressionType, DataBlockIndexType, FifoCompactOptions,
	LruCacheOptions, Options, UniversalCompactOptions, UniversalCompactionStopStyle,
};
use tuwunel_core::{Config, Result, err, utils::math::Expected};

use super::descriptor::{CacheDisp, Descriptor};
use crate::{Context, util::map_err};

pub(super) const SENTINEL_COMPRESSION_LEVEL: i32 = 32767;

/// Adjust options for the specific column by name. Provide the result of
/// db_options() as the argument to this function and use the return value in
/// the arguments to open the specific column.
pub(crate) fn cf_options(ctx: &Context, opts: Options, desc: &Descriptor) -> Result<Options> {
	let cache = get_cache(ctx, desc);
	let config = &ctx.server.config;
	descriptor_cf_options(opts, *desc, config, cache.as_ref())
}

fn descriptor_cf_options(
	mut opts: Options,
	mut desc: Descriptor,
	config: &Config,
	cache: Option<&Cache>,
) -> Result<Options> {
	set_compression(&mut desc, config);
	set_table_options(&mut opts, &desc, cache)?;

	opts.set_min_write_buffer_number(1);
	opts.set_max_write_buffer_number(2);
	opts.set_write_buffer_size(desc.write_size);

	opts.set_target_file_size_base(desc.file_size);
	opts.set_target_file_size_multiplier(desc.file_shape);

	opts.set_level_zero_file_num_compaction_trigger(desc.level0_width);
	opts.set_level_compaction_dynamic_level_bytes(false);
	opts.set_ttl(desc.ttl);

	opts.set_max_bytes_for_level_base(desc.level_size);
	opts.set_max_bytes_for_level_multiplier(1.0);
	opts.set_max_bytes_for_level_multiplier_additional(&desc.level_shape);

	opts.set_disable_auto_compactions(desc.ignored);
	opts.set_compaction_style(desc.compaction);
	opts.set_compaction_pri(desc.compaction_pri);
	opts.set_universal_compaction_options(&uc_options(&desc));
	opts.set_fifo_compaction_options(&fifo_options(&desc));

	let compression_shape: Vec<_> = desc
		.compression_shape
		.into_iter()
		.map(|val| (val > 0).then_some(desc.compression))
		.map(|val| val.unwrap_or(CompressionType::None))
		.collect();

	opts.set_compression_type(desc.compression);
	opts.set_compression_per_level(compression_shape.as_slice());
	opts.set_compression_options(-14, desc.compression_level, 0, 0); // -14 w_bits used by zlib.
	if let Some(&bottommost_level) = desc.bottommost_level.as_ref() {
		opts.set_bottommost_compression_type(desc.compression);
		opts.set_bottommost_zstd_max_train_bytes(0, true);
		opts.set_bottommost_compression_options(
			-14, // -14 w_bits is only read by zlib.
			bottommost_level,
			0,
			0,
			true,
		);
	}

	opts.set_options_from_string("{{arena_block_size=2097152;}}")
		.map_err(map_err)?;

	#[cfg(debug_assertions)]
	opts.set_options_from_string(
		"{{paranoid_checks=true;paranoid_file_checks=true;force_consistency_checks=true;\
		 verify_sst_unique_id_in_manifest=true;}}",
	)
	.map_err(map_err)?;

	Ok(opts)
}

fn set_table_options(opts: &mut Options, desc: &Descriptor, cache: Option<&Cache>) -> Result {
	let mut table = table_options(desc, cache.is_some());

	if let Some(cache) = cache {
		table.set_block_cache(cache);
	} else {
		table.disable_cache();
	}

	let prepopulate = if desc.write_to_cache { "kFlushOnly" } else { "kDisable" };

	let string = format!(
		"{{block_based_table_factory={{num_file_reads_for_auto_readahead={0};\
		 max_auto_readahead_size={1};initial_auto_readahead_size={2};\
		 enable_index_compression={3};prepopulate_block_cache={4}}}}}",
		desc.auto_readahead_thresh,
		desc.auto_readahead_max,
		desc.auto_readahead_init,
		desc.compressed_index,
		prepopulate,
	);

	opts.set_options_from_string(&string)
		.map_err(map_err)?;

	opts.set_block_based_table_factory(&table);

	Ok(())
}

fn set_compression(desc: &mut Descriptor, config: &Config) {
	desc.compression = match config.rocksdb_compression_algo.as_ref() {
		| "snappy" => CompressionType::Snappy,
		| "zlib" => CompressionType::Zlib,
		| "bz2" => CompressionType::Bz2,
		| "lz4" => CompressionType::Lz4,
		| "lz4hc" => CompressionType::Lz4hc,
		| "none" => CompressionType::None,
		| _ => CompressionType::Zstd,
	};

	let can_override_level = config.rocksdb_compression_level == SENTINEL_COMPRESSION_LEVEL
		&& desc.compression == CompressionType::Zstd;

	if !can_override_level {
		desc.compression_level = config.rocksdb_compression_level;
	}

	let can_override_bottom = config.rocksdb_bottommost_compression_level
		== SENTINEL_COMPRESSION_LEVEL
		&& desc.compression == CompressionType::Zstd;

	if !can_override_bottom {
		desc.bottommost_level = Some(config.rocksdb_bottommost_compression_level);
	}

	if !config.rocksdb_bottommost_compression {
		desc.bottommost_level = None;
	}
}

fn fifo_options(desc: &Descriptor) -> FifoCompactOptions {
	let mut opts = FifoCompactOptions::default();
	opts.set_max_table_files_size(desc.limit_size);

	opts
}

fn uc_options(desc: &Descriptor) -> UniversalCompactOptions {
	let mut opts = UniversalCompactOptions::default();
	opts.set_stop_style(UniversalCompactionStopStyle::Total);
	opts.set_min_merge_width(desc.merge_width.0);
	opts.set_max_merge_width(desc.merge_width.1);
	opts.set_max_size_amplification_percent(10000);
	opts.set_compression_size_percent(-1);
	opts.set_size_ratio(1);

	opts
}

fn table_options(desc: &Descriptor, has_cache: bool) -> BlockBasedOptions {
	let mut opts = BlockBasedOptions::default();

	opts.set_block_size(desc.block_size);
	opts.set_metadata_block_size(desc.index_size);

	opts.set_cache_index_and_filter_blocks(has_cache);
	opts.set_pin_top_level_index_and_filter(false);
	opts.set_pin_l0_filter_and_index_blocks_in_cache(false);
	opts.set_partition_pinning_tier(BlockBasedPinningTier::None);
	opts.set_unpartitioned_pinning_tier(BlockBasedPinningTier::None);
	opts.set_top_level_index_pinning_tier(BlockBasedPinningTier::None);

	opts.set_partition_filters(true);
	opts.set_use_delta_encoding(false);
	opts.set_index_type(BlockBasedIndexType::TwoLevelIndexSearch);

	opts.set_data_block_index_type(match desc.block_index_hashing {
		| None if desc.index_size > 512 => DataBlockIndexType::BinaryAndHash,
		| Some(enable) if enable => DataBlockIndexType::BinaryAndHash,
		| Some(_) | None => DataBlockIndexType::BinarySearch,
	});

	opts
}

fn get_cache(ctx: &Context, desc: &Descriptor) -> Option<Cache> {
	if desc.dropped {
		return None;
	}

	// Some cache capacities are overridden by server config in a strange but
	// legacy-compat way
	let config = &ctx.server.config;
	let cap = match desc.name {
		| "eventid_pduid" => Some(config.eventid_pdu_cache_capacity),
		| "eventid_shorteventid" => Some(config.eventidshort_cache_capacity),
		| "shorteventid_eventid" => Some(config.shorteventid_cache_capacity),
		| "shortstatekey_statekey" => Some(config.shortstatekey_cache_capacity),
		| "statekey_shortstatekey" => Some(config.statekeyshort_cache_capacity),
		| "servernameevent_data" => Some(config.servernameevent_data_cache_capacity),
		| "pduid_pdu" | "eventid_outlierpdu" => Some(config.pdu_cache_capacity),
		| "shorteventid_authchain" | "authchainkey_authchain" =>
			Some(config.auth_chain_cache_capacity),
		| _ => None,
	}
	.map(TryInto::try_into)
	.transpose()
	.expect("u32 to usize");

	let ent_size: usize = desc
		.key_size_hint
		.unwrap_or_default()
		.expected_add(desc.val_size_hint.unwrap_or_default());

	let size = match cap {
		| Some(cap) => cache_size(config, cap, ent_size),
		| _ => desc.cache_size,
	};

	let shard_bits: i32 = desc
		.cache_shards
		.ilog2()
		.try_into()
		.expect("u32 to i32 conversion");

	debug_assert!(shard_bits <= 10, "cache shards probably too large");
	let mut cache_opts = LruCacheOptions::default();
	cache_opts.set_num_shard_bits(shard_bits);
	cache_opts.set_capacity(size);

	let mut caches = ctx.col_cache.lock().expect("locked");
	match desc.cache_disp {
		| CacheDisp::Unique if desc.cache_size == 0 => None,
		| CacheDisp::Unique => {
			let cache = Cache::new_lru_cache_opts(&cache_opts);
			caches.insert(desc.name.into(), cache.clone());
			Some(cache)
		},

		| CacheDisp::SharedWith(other) if !caches.contains_key(other) => {
			let cache = Cache::new_lru_cache_opts(&cache_opts);
			caches.insert(desc.name.into(), cache.clone());
			Some(cache)
		},

		| CacheDisp::SharedWith(other) => Some(
			caches
				.get(other)
				.cloned()
				.expect("caches.contains_key(other) must be true"),
		),

		| CacheDisp::Shared => Some(
			caches
				.get("Shared")
				.cloned()
				.expect("shared cache must already exist"),
		),
	}
}

pub(crate) fn cache_size(config: &Config, base_size: u32, entity_size: usize) -> usize {
	cache_size_f64(config, f64::from(base_size), entity_size)
}

#[expect(
	clippy::as_conversions,
	clippy::cast_sign_loss,
	clippy::cast_possible_truncation
)]
pub(crate) fn cache_size_f64(config: &Config, base_size: f64, entity_size: usize) -> usize {
	let ents = base_size * config.cache_capacity_modifier;

	(ents as usize)
		.checked_mul(entity_size)
		.ok_or_else(|| err!(Config("cache_capacity_modifier", "Cache size is too large.")))
		.expect("invalid cache size")
}
