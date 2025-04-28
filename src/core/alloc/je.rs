//! jemalloc allocator

use std::{
	cell::OnceCell,
	ffi::{CStr, c_char, c_void},
	fmt::Debug,
	sync::RwLock,
};

use arrayvec::ArrayVec;
use tikv_jemalloc_ctl as mallctl;
use tikv_jemalloc_sys as ffi;
use tikv_jemallocator as jemalloc;

use crate::{
	Result, err, is_equal_to, is_nonzero,
	utils::{math, math::Tried},
};

#[cfg(feature = "jemalloc_conf")]
#[unsafe(no_mangle)]
pub static malloc_conf: &[u8] = const_str::concat_bytes!(
	"lg_extent_max_active_fit:4",
	",oversize_threshold:16777216",
	",tcache_max:2097152",
	",dirty_decay_ms:16000",
	",muzzy_decay_ms:144000",
	",percpu_arena:percpu",
	",metadata_thp:always",
	",background_thread:true",
	",max_background_threads:-1",
	MALLOC_CONF_PROF,
	0
);

#[cfg(all(feature = "jemalloc_conf", feature = "jemalloc_prof"))]
const MALLOC_CONF_PROF: &str = ",prof_active:false";
#[cfg(all(feature = "jemalloc_conf", not(feature = "jemalloc_prof")))]
const MALLOC_CONF_PROF: &str = "";

#[global_allocator]
static JEMALLOC: jemalloc::Jemalloc = jemalloc::Jemalloc;
static CONTROL: RwLock<()> = RwLock::new(());

type Name = ArrayVec<u8, NAME_MAX>;
type Key = ArrayVec<usize, KEY_SEGS>;

const NAME_MAX: usize = 128;
const KEY_SEGS: usize = 8;

#[crate::ctor]
fn _static_initialization() {
	acq_epoch().expect("pre-initialization of jemalloc failed");
	acq_epoch().expect("pre-initialization of jemalloc failed");
}

#[must_use]
#[cfg(feature = "jemalloc_stats")]
pub fn memory_usage() -> Option<String> {
	use mallctl::stats;

	let mibs = |input: Result<usize, mallctl::Error>| {
		let input = input.unwrap_or_default();
		let kibs = input / 1024;
		let kibs = u32::try_from(kibs).unwrap_or_default();
		let kibs = f64::from(kibs);
		kibs / 1024.0
	};

	// Acquire the epoch; ensure latest stats are pulled in
	acq_epoch().ok()?;

	let allocated = mibs(stats::allocated::read());
	let active = mibs(stats::active::read());
	let mapped = mibs(stats::mapped::read());
	let metadata = mibs(stats::metadata::read());
	let resident = mibs(stats::resident::read());
	let retained = mibs(stats::retained::read());
	Some(format!(
		"allocated: {allocated:.2} MiB\nactive: {active:.2} MiB\nmapped: {mapped:.2} \
		 MiB\nmetadata: {metadata:.2} MiB\nresident: {resident:.2} MiB\nretained: {retained:.2} \
		 MiB\n"
	))
}

#[must_use]
#[cfg(not(feature = "jemalloc_stats"))]
pub fn memory_usage() -> Option<String> {
	None
}

pub fn memory_stats(opts: &str) -> Option<String> {
	const MAX_LENGTH: usize = 1_048_576;

	let mut str = String::new();
	let opaque = std::ptr::from_mut(&mut str).cast::<c_void>();
	let opts_p: *const c_char = std::ffi::CString::new(opts)
		.expect("cstring")
		.into_raw()
		.cast_const();

	// Acquire the epoch; ensure latest stats are pulled in
	acq_epoch().ok()?;

	// SAFETY: calls malloc_stats_print() with our string instance which must remain
	// in this frame. https://docs.rs/tikv-jemalloc-sys/latest/tikv_jemalloc_sys/fn.malloc_stats_print.html
	unsafe { ffi::malloc_stats_print(Some(malloc_stats_cb), opaque, opts_p) };

	str.truncate(MAX_LENGTH);

	Some(str)
}

unsafe extern "C" fn malloc_stats_cb(opaque: *mut c_void, msg: *const c_char) {
	// SAFETY: we have to trust the opaque points to our String
	let res: &mut String = unsafe {
		opaque
			.cast::<String>()
			.as_mut()
			.expect("failed to cast void* to &mut String")
	};

	// SAFETY: we have to trust the string is null terminated.
	let msg = unsafe { CStr::from_ptr(msg) };

	let msg = String::from_utf8_lossy(msg.to_bytes());
	res.push_str(msg.as_ref());
}

macro_rules! mallctl {
	($name:expr_2021) => {{
		thread_local! {
			static KEY: OnceCell<Key> = OnceCell::default();
		};

		KEY.with(|once| {
			once.get_or_init(move || key($name).expect("failed to translate name into mib key"))
				.clone()
		})
	}};
}

pub mod this_thread {
	use super::{Debug, Key, OnceCell, Result, is_nonzero, key, math};

	thread_local! {
		static ALLOCATED_BYTES: OnceCell<&'static u64> = const { OnceCell::new() };
		static DEALLOCATED_BYTES: OnceCell<&'static u64> = const { OnceCell::new() };
	}

	pub fn trim() -> Result {
		decay().and_then(|()| purge())
	}

	pub fn purge() -> Result {
		notify(mallctl!("arena.0.purge"))
	}

	pub fn decay() -> Result {
		notify(mallctl!("arena.0.decay"))
	}

	pub fn idle() -> Result {
		super::notify(&mallctl!("thread.idle"))
	}

	pub fn flush() -> Result {
		super::notify(&mallctl!("thread.tcache.flush"))
	}

	pub fn set_muzzy_decay(decay_ms: isize) -> Result<isize> {
		set(mallctl!("arena.0.muzzy_decay_ms"), decay_ms)
	}

	pub fn get_muzzy_decay() -> Result<isize> {
		get(mallctl!("arena.0.muzzy_decay_ms"))
	}

	pub fn set_dirty_decay(decay_ms: isize) -> Result<isize> {
		set(mallctl!("arena.0.dirty_decay_ms"), decay_ms)
	}

	pub fn get_dirty_decay() -> Result<isize> {
		get(mallctl!("arena.0.dirty_decay_ms"))
	}

	pub fn cache_enable(enable: bool) -> Result<bool> {
		super::set::<u8>(&mallctl!("thread.tcache.enabled"), enable.into()).map(is_nonzero!())
	}

	pub fn is_cache_enabled() -> Result<bool> {
		super::get::<u8>(&mallctl!("thread.tcache.enabled")).map(is_nonzero!())
	}

	pub fn set_arena(id: usize) -> Result<usize> {
		super::set::<u32>(&mallctl!("thread.arena"), id.try_into()?).and_then(math::try_into)
	}

	pub fn arena_id() -> Result<usize> {
		super::get::<u32>(&mallctl!("thread.arena")).and_then(math::try_into)
	}

	pub fn prof_enable(enable: bool) -> Result<bool> {
		super::set::<u8>(&mallctl!("thread.prof.active"), enable.into()).map(is_nonzero!())
	}

	pub fn is_prof_enabled() -> Result<bool> {
		super::get::<u8>(&mallctl!("thread.prof.active")).map(is_nonzero!())
	}

	pub fn reset_peak() -> Result {
		super::notify(&mallctl!("thread.peak.reset"))
	}

	pub fn peak() -> Result<u64> {
		super::get(&mallctl!("thread.peak.read"))
	}

	#[inline]
	#[must_use]
	pub fn allocated() -> u64 {
		*ALLOCATED_BYTES.with(|once| init_tls_cell(once, "thread.allocatedp"))
	}

	#[inline]
	#[must_use]
	pub fn deallocated() -> u64 {
		*DEALLOCATED_BYTES.with(|once| init_tls_cell(once, "thread.deallocatedp"))
	}

	fn notify(key: Key) -> Result {
		super::notify_by_arena(Some(arena_id()?), key)
	}

	fn set<T>(key: Key, val: T) -> Result<T>
	where
		T: Copy + Debug,
	{
		super::set_by_arena(Some(arena_id()?), key, val)
	}

	fn get<T>(key: Key) -> Result<T>
	where
		T: Copy + Debug,
	{
		super::get_by_arena(Some(arena_id()?), key)
	}

	fn init_tls_cell(cell: &OnceCell<&'static u64>, name: &str) -> &'static u64 {
		cell.get_or_init(|| {
			let ptr: *const u64 = super::get(&mallctl!(name)).expect("failed to obtain pointer");

			// SAFETY: ptr points directly to the internal state of jemalloc for this thread
			unsafe { ptr.as_ref() }.expect("pointer must not be null")
		})
	}
}

pub fn stats_reset() -> Result {
	notify(&mallctl!("stats.mutexes.reset"))
}

pub fn prof_reset() -> Result {
	notify(&mallctl!("prof.reset"))
}

pub fn prof_enable(enable: bool) -> Result<bool> {
	set::<u8>(&mallctl!("prof.active"), enable.into()).map(is_nonzero!())
}

pub fn is_prof_enabled() -> Result<bool> {
	get::<u8>(&mallctl!("prof.active")).map(is_nonzero!())
}

pub fn trim<I: Into<Option<usize>> + Copy>(arena: I) -> Result {
	decay(arena).and_then(|()| purge(arena))
}

pub fn purge<I: Into<Option<usize>>>(arena: I) -> Result {
	notify_by_arena(arena.into(), mallctl!("arena.4096.purge"))
}

pub fn decay<I: Into<Option<usize>>>(arena: I) -> Result {
	notify_by_arena(arena.into(), mallctl!("arena.4096.decay"))
}

pub fn set_muzzy_decay<I: Into<Option<usize>>>(arena: I, decay_ms: isize) -> Result<isize> {
	match arena.into() {
		| Some(arena) => {
			set_by_arena(Some(arena), mallctl!("arena.4096.muzzy_decay_ms"), decay_ms)
		},
		| _ => set(&mallctl!("arenas.muzzy_decay_ms"), decay_ms),
	}
}

pub fn set_dirty_decay<I: Into<Option<usize>>>(arena: I, decay_ms: isize) -> Result<isize> {
	match arena.into() {
		| Some(arena) => {
			set_by_arena(Some(arena), mallctl!("arena.4096.dirty_decay_ms"), decay_ms)
		},
		| _ => set(&mallctl!("arenas.dirty_decay_ms"), decay_ms),
	}
}

#[inline]
#[must_use]
pub fn is_affine_arena() -> bool {
	is_percpu_arena() || is_phycpu_arena()
}

#[inline]
#[must_use]
pub fn is_percpu_arena() -> bool {
	percpu_arenas().is_ok_and(is_equal_to!("percpu"))
}

#[inline]
#[must_use]
pub fn is_phycpu_arena() -> bool {
	percpu_arenas().is_ok_and(is_equal_to!("phycpu"))
}

pub fn percpu_arenas() -> Result<&'static str> {
	let ptr = get::<*const c_char>(&mallctl!("opt.percpu_arena"))?;
	//SAFETY: ptr points to a null-terminated string returned for opt.percpu_arena.
	let cstr = unsafe { CStr::from_ptr(ptr) };
	cstr.to_str().map_err(Into::into)
}

pub fn arenas() -> Result<usize> {
	get::<u32>(&mallctl!("arenas.narenas")).and_then(math::try_into)
}

pub fn inc_epoch() -> Result<u64> {
	xchg(&mallctl!("epoch"), 1_u64)
}

pub fn acq_epoch() -> Result<u64> {
	xchg(&mallctl!("epoch"), 0_u64)
}

fn notify_by_arena(id: Option<usize>, mut key: Key) -> Result {
	key[1] = id.unwrap_or(4096);
	notify(&key)
}

fn set_by_arena<T>(id: Option<usize>, mut key: Key, val: T) -> Result<T>
where
	T: Copy + Debug,
{
	key[1] = id.unwrap_or(4096);
	set(&key, val)
}

fn get_by_arena<T>(id: Option<usize>, mut key: Key) -> Result<T>
where
	T: Copy + Debug,
{
	key[1] = id.unwrap_or(4096);
	get(&key)
}

fn notify(key: &Key) -> Result {
	xchg(key, ())
}

fn set<T>(key: &Key, val: T) -> Result<T>
where
	T: Copy + Debug,
{
	let _lock = CONTROL.write()?;
	let res = xchg(key, val)?;
	inc_epoch()?;

	Ok(res)
}

#[tracing::instrument(
	name = "get",
	level = "trace"
	skip_all,
	fields(?key)
)]
fn get<T>(key: &Key) -> Result<T>
where
	T: Copy + Debug,
{
	acq_epoch()?;
	acq_epoch()?;

	// SAFETY: T must be perfectly valid to receive value.
	unsafe { mallctl::raw::read_mib(key.as_slice()) }.map_err(map_err)
}

#[tracing::instrument(
	name = "xchg",
	level = "trace"
	skip_all,
	fields(?key, ?val)
)]
fn xchg<T>(key: &Key, val: T) -> Result<T>
where
	T: Copy + Debug,
{
	// SAFETY: T must be the exact expected type.
	unsafe { mallctl::raw::update_mib(key.as_slice(), val) }.map_err(map_err)
}

fn key(name: &str) -> Result<Key> {
	// tikv asserts the output buffer length is tight to the number of required mibs
	// so we slice that down here.
	let segs = name
		.chars()
		.filter(is_equal_to!(&'.'))
		.count()
		.try_add(1)?;

	let name = self::name(name)?;
	let mut buf = [0_usize; KEY_SEGS];
	mallctl::raw::name_to_mib(name.as_slice(), &mut buf[0..segs])
		.map_err(map_err)
		.map(move |()| buf.into_iter().take(segs).collect())
}

fn name(name: &str) -> Result<Name> {
	let mut buf = Name::new();
	buf.try_extend_from_slice(name.as_bytes())?;
	buf.try_extend_from_slice(b"\0")?;

	Ok(buf)
}

fn map_err(error: tikv_jemalloc_ctl::Error) -> crate::Error {
	err!("mallctl: {}", error.to_string())
}
