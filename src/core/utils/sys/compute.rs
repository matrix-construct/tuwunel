//! System utilities related to compute/processing

use std::{cell::Cell, fmt::Debug, path::PathBuf, sync::LazyLock};

use crate::{Result, is_equal_to};

type Id = usize;

type Mask = u128;
type Masks = [Mask; MASK_BITS];

const MASK_BITS: usize = CORES_MAX;

/// Maximum number of cores we support; for now limited to bits of our mask
/// integral.
pub const CORES_MAX: usize = 128;

/// The mask of logical cores available to the process (at startup).
static CORES_AVAILABLE: LazyLock<Mask> = LazyLock::new(|| into_mask(query_cores_available()));

/// Stores the mask of logical-cores with thread/HT/SMT association. Each group
/// here makes up a physical-core.
static SMT_TOPOLOGY: LazyLock<Masks> = LazyLock::new(init_smt_topology);

/// Stores the mask of logical-core associations on a node/socket. Bits are set
/// for all logical cores within all physical cores of the node.
static NODE_TOPOLOGY: LazyLock<Masks> = LazyLock::new(init_node_topology);

thread_local! {
	/// Tracks the affinity for this thread. This is updated when affinities
	/// are set via our set_affinity() interface.
	static CORE_AFFINITY: Cell<Mask> = const { Cell::new(0) };
}

/// Set the core affinity for this thread. The ID should be listed in
/// CORES_AVAILABLE. Empty input is a no-op; prior affinity unchanged.
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		id = ?std::thread::current().id(),
		name = %std::thread::current().name().unwrap_or("None"),
		set = ?ids.clone().collect::<Vec<_>>(),
		CURRENT = %format!("[b{:b}]", CORE_AFFINITY.get()),
		AVAILABLE = %format!("[b{:b}]", *CORES_AVAILABLE),
	),
)]
pub fn set_affinity<I>(mut ids: I)
where
	I: Iterator<Item = Id> + Clone + Debug,
{
	use core_affinity::{CoreId, set_each_for_current, set_for_current};

	let n = ids.clone().count();
	let mask: Mask = ids.clone().fold(0, |mask, id| {
		debug_assert!(is_core_available(id), "setting affinity to unavailable core");
		mask | (1 << id)
	});

	if n > 1 {
		set_each_for_current(ids.map(|id| CoreId { id }));
	} else if n > 0 {
		set_for_current(CoreId { id: ids.next().expect("n > 0") });
	}

	if mask.count_ones() > 0 {
		CORE_AFFINITY.replace(mask);
	}
}

/// Get the core affinity for this thread.
pub fn get_affinity() -> impl Iterator<Item = Id> {
	CORE_AFFINITY
		.get()
		.ne(&0)
		.then_some(from_mask(CORE_AFFINITY.get()))
		.or_else(|| Some(from_mask(*CORES_AVAILABLE)))
		.into_iter()
		.flatten()
}

/// List the cores sharing SMT-tier resources
pub fn smt_siblings() -> impl Iterator<Item = Id> {
	from_mask(get_affinity().fold(0_u128, |mask, id| {
		mask | SMT_TOPOLOGY
			.get(id)
			.expect("ID must not exceed max cpus")
	}))
}

/// List the cores sharing Node-tier resources relative to this threads current
/// affinity.
pub fn node_siblings() -> impl Iterator<Item = Id> {
	from_mask(get_affinity().fold(0_u128, |mask, id| {
		mask | NODE_TOPOLOGY
			.get(id)
			.expect("Id must not exceed max cpus")
	}))
}

/// Get the cores sharing SMT resources relative to id.
#[inline]
pub fn smt_affinity(id: Id) -> impl Iterator<Item = Id> {
	from_mask(
		*SMT_TOPOLOGY
			.get(id)
			.expect("ID must not exceed max cpus"),
	)
}

/// Get the cores sharing Node resources relative to id.
#[inline]
pub fn node_affinity(id: Id) -> impl Iterator<Item = Id> {
	from_mask(
		*NODE_TOPOLOGY
			.get(id)
			.expect("ID must not exceed max cpus"),
	)
}

/// Get the number of threads which could execute in parallel based on hardware
/// constraints of this system.
#[inline]
#[must_use]
pub fn available_parallelism() -> usize { cores_available().count() }

/// Gets the ID of the nth core available. This bijects our sequence of cores to
/// actual ID's which may have gaps for cores which are not available.
#[inline]
#[must_use]
pub fn nth_core_available(i: usize) -> Option<Id> { cores_available().nth(i) }

/// Determine if core (by id) is available to the process.
#[inline]
#[must_use]
pub fn is_core_available(id: Id) -> bool { cores_available().any(is_equal_to!(id)) }

/// Get the list of cores available. The values were recorded at program start.
#[inline]
pub fn cores_available() -> impl Iterator<Item = Id> { from_mask(*CORES_AVAILABLE) }

#[cfg(target_os = "linux")]
#[inline]
pub fn getcpu() -> Result<usize> {
	use crate::{Error, utils::math};

	// SAFETY: This is part of an interface with many low-level calls taking many
	// raw params, but it's unclear why this specific call is unsafe. Nevertheless
	// the value obtained here is semantically unsafe because it can change on the
	// instruction boundary trailing its own acquisition and also any other time.
	let ret: i32 = unsafe { libc::sched_getcpu() };

	#[cfg(target_os = "linux")]
	// SAFETY: On modern linux systems with a vdso if we can optimize away the branch checking
	// for error (see getcpu(2)) then this system call becomes a memory access.
	unsafe {
		std::hint::assert_unchecked(ret >= 0);
	};

	if ret == -1 {
		return Err(Error::from_errno());
	}

	math::try_into(ret)
}

#[cfg(not(target_os = "linux"))]
#[inline]
pub fn getcpu() -> Result<usize> { Err(crate::Error::Io(std::io::ErrorKind::Unsupported.into())) }

fn query_cores_available() -> impl Iterator<Item = Id> {
	core_affinity::get_core_ids()
		.unwrap_or_default()
		.into_iter()
		.map(|core_id| core_id.id)
}

fn init_smt_topology() -> [Mask; MASK_BITS] { [Mask::default(); MASK_BITS] }

fn init_node_topology() -> [Mask; MASK_BITS] { [Mask::default(); MASK_BITS] }

fn into_mask<I>(ids: I) -> Mask
where
	I: Iterator<Item = Id>,
{
	ids.inspect(|&id| {
		debug_assert!(id < MASK_BITS, "Core ID must be < Mask::BITS at least for now");
	})
	.fold(Mask::default(), |mask, id| mask | (1 << id))
}

fn from_mask(v: Mask) -> impl Iterator<Item = Id> {
	(0..MASK_BITS).filter(move |&i| (v & (1 << i)) != 0)
}

fn _sys_path(id: usize, suffix: &str) -> PathBuf {
	format!("/sys/devices/system/cpu/cpu{id}/{suffix}").into()
}
