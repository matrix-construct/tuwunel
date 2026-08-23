use std::{path::PathBuf, sync::Arc};

use tuwunel_core::{
	Config, Server, at, debug,
	debug::INFO_SPAN_LEVEL,
	debug_info, debug_warn, expected, info, is_equal_to,
	utils::{
		BoolExt,
		math::usize_from_f64,
		result::LogDebugErr,
		stream,
		stream::{AMPLIFICATION_LIMIT, WIDTH_LIMIT},
		sys::{
			compute::{available_parallelism, cores_available, is_core_available},
			max_threads,
			storage::{self, MultiDevice},
		},
	},
};

use super::{QUEUE_LIMIT, WORKER_LIMIT};

/// Determine storage hardware capabilities of the system for configuring the
/// shape of the database frontend threadpool.
///
/// Returns a tuple of:
/// - `topology` Vector mapping hardware cores to hardware queues. Systems with
///   fewer queues than cores will see queue ID's repeated. Systems with the
///   same or more queues as cores will usually see a 1:1 association of core
///   ID's to queue ID's. Systems with sparse core assignments will see 0 for
///   core ID positions not available to the process. Systems where detection
///   failed will see a default of 1:1 core identity as a best-guess maintaining
///   core locality.
/// - `workers` Vector mapping hardware queues to the number of threads to spawn
///   in service of that queue. Systems with fewer queues than cores will set an
///   affinity mask for each thread to multiple cores based on the topology.
///   Systems with equal or more hardware queues than cores will set a single
///   affinity for each thread.
/// - `queues` Vector of software mpmc queues to create and the size of each
///   queue. Each indice is associated with a thread-pool of workers which it
///   feeds requests from various tokio tasks. When this queue reaches capacity
///   the tokio task must yield.
#[tracing::instrument(
	level = INFO_SPAN_LEVEL,
	skip_all,
	ret(level = "trace"),
)]
pub(super) fn configure(server: &Arc<Server>) -> (Vec<usize>, Vec<usize>, Vec<usize>) {
	let config = &server.config;
	let num_cores = available_parallelism();

	let cores_max = cores_available()
		.last()
		.unwrap_or(0)
		.saturating_add(1);

	let path: PathBuf = config.database_path.clone();
	let device_name = storage::name_from_path(&path)
		.log_debug_err()
		.ok();

	let devices = storage::md_discover(&path);
	let topology_detected = devices.md.is_empty().is_false();
	debug!(?topology_detected, ?device_name, ?devices);

	let default_worker_count = topology_detected
		.is_false()
		.then_some(config.db_pool_workers)
		.map(|workers| workers.saturating_mul(num_cores));

	let total_tags = sum_total_tags(&devices, default_worker_count);
	let topology = compute_topology(&devices, topology_detected, cores_max);
	let max_workers =
		compute_max_workers(&devices, default_worker_count, config.db_pool_max_workers);

	let chan_limit = expected!(max_workers / num_cores)
		.saturating_sub(8)
		.saturating_add(1)
		.next_multiple_of(8);

	let workers =
		compute_workers(&devices, config, default_worker_count, topology.len(), chan_limit);

	let workers = constrain_workers(workers, max_workers);

	let queues: Vec<usize> = workers
		.iter()
		.map(|count| {
			count
				.saturating_mul(config.db_pool_queue_mult)
				.min(QUEUE_LIMIT.1)
		})
		.collect();

	let total_workers = workers.iter().sum::<usize>();
	let total_capacity = queues.iter().sum::<usize>();
	let num_queues = queues.iter().filter(|&&cap| cap > 0).count();

	if config.stream_width_scale > 0.0 {
		update_stream_width(server, num_queues, total_workers, total_capacity);
	}

	log_topology(
		topology_detected,
		device_name.as_deref(),
		num_cores,
		&topology,
		&workers,
		&queues,
		num_queues,
		total_workers,
		total_tags,
		total_capacity,
	);

	assert!(total_workers > 0, "some workers expected");
	// A queue's capacity is positive exactly when its group is populated, so
	// num_queues is the floor constrain_workers guarantees.
	debug_assert!(total_workers <= max_workers.max(num_queues), "spawning too many workers");

	assert!(!queues.is_empty(), "some queues expected");
	assert!(!queues.iter().copied().all(is_equal_to!(0)), "positive queue capacity expected");

	(topology, workers, queues)
}

/// Sum the total number of possible tags. Without hardware detection this
/// reduces to the default worker count. The thread-worker model never
/// approaches actual NVMe capacity, but the value still informs request
/// capacity downstream.
fn sum_total_tags(devices: &MultiDevice, default_worker_count: Option<usize>) -> usize {
	devices
		.md
		.iter()
		.flat_map(|md| md.mq.iter())
		.filter(|mq| mq.cpu_list.iter().copied().any(is_core_available))
		.filter_map(|mq| mq.nr_tags)
		.chain(default_worker_count)
		.fold(0_usize, usize::saturating_add)
}

/// Map cores to their associated hardware queue. Shared queues repeat across
/// cores; sparse unavailable cores default to 0; undetected hardware falls back
/// to the core identity as a best-guess maintaining core locality.
fn compute_topology(
	devices: &MultiDevice,
	topology_detected: bool,
	cores_max: usize,
) -> Vec<usize> {
	devices
		.md
		.iter()
		.flat_map(|md| md.mq.iter())
		.fold(vec![0; cores_max], |mut topology, mq| {
			mq.cpu_list
				.iter()
				.filter(|&&id| id < cores_max)
				.filter(|&&id| is_core_available(id))
				.for_each(|&id| {
					topology[id] = mq.id;
				});

			topology
		})
		.into_iter()
		.enumerate()
		.map(|(core_id, queue_id)| {
			topology_detected
				.then_some(queue_id)
				.unwrap_or(core_id)
		})
		.collect()
}

/// Determine an ideal max worker count based on true capacity. The true value
/// is rarely attainable in a thread-worker model so the result is clamped by
/// both the rlimit-derived budget and the static `WORKER_LIMIT` range.
fn compute_max_workers(
	devices: &MultiDevice,
	default_worker_count: Option<usize>,
	max_workers_cfg: usize,
) -> usize {
	let max_threads = max_threads()
		.map(at!(0))
		.unwrap_or(usize::MAX)
		.saturating_div(3);

	devices
		.md
		.iter()
		.flat_map(|md| md.mq.iter())
		.filter_map(|mq| mq.nr_tags)
		.chain(default_worker_count)
		.fold(0_usize, usize::saturating_add)
		.min(max_workers_cfg)
		.clamp(WORKER_LIMIT.0, max_threads)
		.clamp(WORKER_LIMIT.0, WORKER_LIMIT.1)
}

/// Determine the worker groupings. Each indice represents a hardware queue and
/// contains the number of workers which will service it. The vector is
/// truncated to the number of cores on systems with multiple hardware queues
/// per core, and the per-pool count is capped well below NVMe capacity.
fn compute_workers(
	devices: &MultiDevice,
	config: &Config,
	default_worker_count: Option<usize>,
	topology_len: usize,
	chan_limit: usize,
) -> Vec<usize> {
	let default_workers = default_worker_count
		.into_iter()
		.cycle()
		.enumerate()
		.map(move |(core_id, count)| {
			is_core_available(core_id)
				.then_some(count)
				.unwrap_or(0)
				.min(chan_limit)
		});

	devices
		.md
		.iter()
		.inspect(|md| debug!(?md))
		.flat_map(|md| md.mq.iter())
		.map(|mq| {
			let shares = mq
				.cpu_list
				.iter()
				.filter(|&&id| is_core_available(id))
				.count();

			let conf_limit = config
				.db_pool_workers_limit
				.saturating_mul(shares);

			let hard_limit = devices
				.md
				.iter()
				.filter(|_| shares > 0)
				.fold(0_usize, |acc, mq| {
					mq.nr_requests
						.map(|nr| nr.min(conf_limit))
						.or(Some(conf_limit))
						.map(|nr| acc.saturating_add(nr))
						.unwrap_or(acc)
				});

			let tags = mq
				.nr_tags
				.unwrap_or(WORKER_LIMIT.0)
				.min(hard_limit)
				.min(chan_limit);

			debug!(?mq, ?shares, ?tags, ?conf_limit, ?hard_limit, ?chan_limit);

			tags
		})
		.chain(default_workers)
		.take(topology_len)
		.collect()
}

/// Reduce the worker groups until their total satisfies the configured
/// maximum.
///
/// Group counts are levelled down toward an even share of the maximum, with
/// the remainder spread one apiece over the leading populated groups. Every
/// populated group retains at least one worker so its queue keeps service,
/// making the number of populated groups the effective floor.
fn constrain_workers(mut workers: Vec<usize>, max_workers: usize) -> Vec<usize> {
	let total: usize = workers.iter().sum();

	if total <= max_workers {
		return workers;
	}

	let num_groups = workers.iter().filter(|&&count| count > 0).count();

	let share = expected!(max_workers / num_groups);
	let extra = expected!(max_workers % num_groups);

	workers
		.iter_mut()
		.filter(|count| **count > 0)
		.enumerate()
		.for_each(|(rank, count)| {
			*count = share
				.saturating_add(usize::from(rank < extra))
				.max(1)
				.min(*count);
		});

	workers
}

#[expect(clippy::too_many_arguments)]
fn log_topology(
	topology_detected: bool,
	device_name: Option<&str>,
	num_cores: usize,
	topology: &[usize],
	workers: &[usize],
	queues: &[usize],
	num_queues: usize,
	total_workers: usize,
	total_tags: usize,
	total_capacity: usize,
) {
	if topology_detected {
		debug_info!(?num_cores, ?topology, ?workers, ?queues, "Frontend topology",);
		info!(
			device_name = ?device_name.unwrap_or("None"),
			?num_queues,
			?total_workers,
			?total_tags,
			?total_capacity,
			stream_width = ?stream::automatic_width(),
			amplification = ?stream::automatic_amplification(),
			"Frontend topology",
		);
	} else {
		debug_info!(?num_cores, ?topology, ?workers, ?queues, "Frontend topology (defaults)");
		debug_warn!(
			device_name = ?device_name.unwrap_or("None"),
			?total_workers,
			?total_capacity,
			stream_width = ?stream::automatic_width(),
			amplification = ?stream::automatic_amplification(),
			"Storage hardware not detected for database directory; assuming defaults.",
		);
	}
}

#[expect(clippy::as_conversions, clippy::cast_precision_loss)]
fn update_stream_width(
	server: &Arc<Server>,
	num_queues: usize,
	total_workers: usize,
	_total_capacity: usize,
) {
	assert!(num_queues > 0, "Expected at least one queue.");
	assert!(total_workers > 0, "Expected some workers.");

	let config = &server.config;
	let scale: f64 = config.stream_width_scale.min(100.0).into();
	let max_width = expected!(total_workers / num_queues);

	let old_width = stream::automatic_width();
	let old_scale_width = expected!(old_width * num_queues);

	let new_scale = total_workers as f64 / old_scale_width as f64;
	let new_scale = new_scale.clamp(1.0, 4.0);
	let new_scale_width = new_scale * old_width as f64;
	let new_scale_width = usize_from_f64(new_scale_width)
		.expect("failed to convert f64 to usize")
		.next_multiple_of(8);

	let req_width = usize_from_f64(scale * new_scale_width as f64)
		.expect("failed to convert f64 to usize")
		.next_multiple_of(4)
		.min(max_width)
		.clamp(WIDTH_LIMIT.0, WIDTH_LIMIT.1);

	let req_amp = new_scale * config.stream_amplification as f64;
	let req_amp = usize_from_f64(req_amp * scale)
		.expect("failed to convert f64 to usize")
		.next_multiple_of(64)
		.clamp(AMPLIFICATION_LIMIT.0, AMPLIFICATION_LIMIT.1);

	let (old_width, new_width) = stream::set_width(req_width);
	let (old_amp, new_amp) = stream::set_amplification(req_amp);
	debug!(
		config_scale = ?config.stream_width_scale,
		?old_width,
		?new_scale,
		?new_width,
		?old_amp,
		?new_amp,
		"Updated global stream width"
	);
}

#[cfg(test)]
mod tests {
	use super::constrain_workers;

	#[test]
	fn constrain_respects_the_maximum() {
		let constrained = constrain_workers(vec![8; 24], 64);

		assert_eq!(constrained.iter().sum::<usize>(), 64);
		assert!(constrained.iter().all(|&count| count > 0));
	}

	#[test]
	fn constrain_floors_populated_groups() {
		assert_eq!(constrain_workers(vec![8, 0, 8, 8], 2), vec![1, 0, 1, 1]);
	}

	#[test]
	fn constrain_passes_a_satisfied_total() {
		let workers = vec![4, 0, 4];

		assert_eq!(constrain_workers(workers.clone(), 8), workers);
	}
}
