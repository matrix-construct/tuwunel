//! jemalloc allocator

use std::{
	alloc::Layout,
	ffi::{CStr, c_char, c_void},
	io::Write,
	panic::catch_unwind,
	process::abort,
	sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

// A control that collides with the wrapper adapting its error type below is
// aliased on import.
use jevmalloc::{
	Jemalloc,
	ctl::{
		Error as CtlError, background_thread_enable as ctl_background_thread_enable,
		refresh_epoch, trim as ctl_trim,
	},
	ffi,
	global_alloc::hook::{ALLOC, ALLOC_ZEROED},
};
use libc::{STDOUT_FILENO, write};

use crate::{Result, arrayvec::ArrayVec, err, utils::BoolExt};

/// Line buffer for one allocation-trace record. A record of three integers at
/// their maximum widths occupies 74 bytes.
type TraceLine = ArrayVec<u8, 128>;

/// Provides the process-wide jemalloc startup configuration.
///
/// Jemalloc reads this unmangled symbol during allocator initialization, which
/// can occur before `main`. The NUL-terminated options enable CPU-affine
/// arenas, background purging, metadata huge pages, and tuned cache and decay
/// thresholds.
#[cfg(feature = "jemalloc_conf")]
#[used]
#[unsafe(no_mangle)]
pub static malloc_conf: &[u8] = const_str::concat_bytes!(
	"tcache:true",
	",percpu_arena:percpu",
	",metadata_thp:always",
	",background_thread:true",
	",max_background_threads:-1",
	",lg_extent_max_active_fit:4",
	",oversize_threshold:2097152",
	",tcache_max:524288",
	",dirty_decay_ms:16000",
	",muzzy_decay_ms:144000",
	//MALLOC_CONF_PROF,
	0
);

#[cfg(all(
	feature = "jemalloc_conf",
	feature = "jemalloc_prof",
	target_arch = "x86_64",
))]
const _MALLOC_CONF_PROF: &str = ",prof_active:false";
#[cfg(all(
	feature = "jemalloc_conf",
	any(not(feature = "jemalloc_prof"), not(target_arch = "x86_64")),
))]
const _MALLOC_CONF_PROF: &str = "";

#[global_allocator]
static JEMALLOC: Jemalloc = Jemalloc;

static GLOBAL_ALLOCS: AtomicU64 = AtomicU64::new(0);
static COUNT_GLOBAL_ALLOCS: AtomicBool = AtomicBool::new(false);
static TRACE_GLOBAL_ALLOCS: AtomicBool = AtomicBool::new(false);

/// Registers the allocation-observer callbacks during process startup.
///
/// Normal and zeroed allocations made after registration feed the same counting
/// and tracing instrumentation. The allocator reads these slots only when
/// `jevmalloc` is built with its `global_hooks` feature.
#[crate::ctor(unsafe)]
fn _static_initialization() {
	// SAFETY: Mutable static globals in jemalloc crate; must be initialized
	// properly and uniquely.
	unsafe { ALLOC = Some(global_alloc_hook) };

	// SAFETY: As above.
	unsafe { ALLOC_ZEROED = Some(global_alloc_zeroed_hook) };
}

fn global_alloc_hook(layout: Layout) {
	catch_unwind(move || handle_global_alloc(layout))
		.map_err(|_| abort())
		.ok();
}

fn global_alloc_zeroed_hook(layout: Layout) {
	catch_unwind(move || handle_global_alloc(layout))
		.map_err(|_| abort())
		.ok();
}

fn handle_global_alloc(layout: Layout) {
	let do_count = COUNT_GLOBAL_ALLOCS.load(Ordering::Relaxed);
	let count = GLOBAL_ALLOCS.fetch_add(do_count.into(), Ordering::Relaxed);

	if TRACE_GLOBAL_ALLOCS.load(Ordering::Relaxed) {
		let mut buf = TraceLine::new();

		writeln!(&mut buf, "{count} align={} size={}", layout.align(), layout.size())
			.expect("writeln! to buffer failed");

		// SAFETY: Valid ptr and len from buf for writing to stdout.
		unsafe { write(STDOUT_FILENO, buf.as_ptr().cast::<c_void>(), buf.len()) }
			.ge(&0)
			.into_result()
			.expect("write(2) error");
	}
}

/// Returns the process allocation count observed by the allocator hook.
///
/// The counter uses relaxed ordering and advances only when internal allocation
/// counting is enabled. It is intended for allocation measurements rather than
/// synchronized accounting.
#[inline]
#[must_use]
pub fn global_alloc_count() -> u64 { GLOBAL_ALLOCS.load(Ordering::Relaxed) }

/// Returns a human-readable snapshot of allocator memory usage when available.
///
/// Detailed summary reporting is currently disabled for this build, so this
/// implementation returns `None`. Raw allocator output remains available
/// through [`memory_stats`].
#[must_use]
pub fn memory_usage() -> Option<String> { None }

/// Collects jemalloc's raw statistics report with the supplied print options.
///
/// The allocator statistics epoch is refreshed before invoking
/// `malloc_stats_print`. Refresh failure returns `None`, and output is
/// truncated to 1 MiB.
///
/// # Panics
///
/// Panics if `opts` contains an interior NUL byte.
pub fn memory_stats(opts: &str) -> Option<String> {
	const MAX_LENGTH: usize = 1_048_576;

	let mut str = String::new();
	let opaque = std::ptr::from_mut(&mut str).cast::<c_void>();
	let opts_p: *const c_char = std::ffi::CString::new(opts)
		.expect("cstring")
		.into_raw()
		.cast_const();

	// Acquire the epoch; ensure latest stats are pulled in
	refresh_epoch().ok()?;

	// SAFETY: calls malloc_stats_print() with our string instance which must remain
	// in this frame. https://docs.rs/tikv-jemalloc-sys/latest/tikv_jemalloc_sys/fn.malloc_stats_print.html
	unsafe { ffi::malloc_stats_print(Some(malloc_stats_cb), opaque, opts_p) };

	str.truncate(MAX_LENGTH);

	Some(str)
}

/// Appends a jemalloc statistics fragment through the C callback boundary.
///
/// Panics are caught and converted into process aborts before control returns
/// to jemalloc.
///
/// # Safety
///
/// `opaque` must point to the live `String` supplied by [`memory_stats`], and
/// `msg` must point to a NUL-terminated string valid for the duration of the
/// call.
unsafe extern "C" fn malloc_stats_cb(opaque: *mut c_void, msg: *const c_char) {
	catch_unwind(move || handle_malloc_stats(opaque, msg))
		.map_err(|_| abort())
		.ok();
}

fn handle_malloc_stats(opaque: *mut c_void, msg: *const c_char) {
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

/// Reclaims unused pages from one arena or from all arenas.
///
/// The operation applies time-based decay before purging remaining unused dirty
/// pages. Passing `None` selects every arena.
pub fn trim<I: Into<Option<usize>>>(arena: I) -> Result { ctl_trim(arena).map_err(map_err) }

/// Enables or disables jemalloc background purge threads.
///
/// Disabling waits for the workers to terminate before returning. The previous
/// setting is returned.
pub fn background_thread_enable(enable: bool) -> Result<bool> {
	ctl_background_thread_enable(enable).map_err(map_err)
}

/// Controls jemalloc state associated with the calling thread.
///
/// These wrappers resolve the thread's own arena before applying the operation.
/// Control failures are returned through the crate's allocator error type.
pub mod this_thread {
	use jevmalloc::ctl::this_thread::{
		decay as ctl_decay, set_muzzy_decay as ctl_set_muzzy_decay,
	};

	use super::{Result, map_err};

	/// Applies decay-based purging to the calling thread's arena.
	///
	/// Jemalloc selects unused dirty and muzzy pages according to their
	/// configured decay intervals.
	pub fn decay() -> Result { ctl_decay().map_err(map_err) }

	/// Sets the calling thread's arena muzzy-page decay interval.
	///
	/// A value of `0` requests immediate purging, while `-1` disables purging.
	/// The previous interval is returned.
	pub fn set_muzzy_decay(decay_ms: isize) -> Result<isize> {
		ctl_set_muzzy_decay(decay_ms).map_err(map_err)
	}
}

fn map_err(error: CtlError) -> crate::Error { err!("mallctl: {error}") }
