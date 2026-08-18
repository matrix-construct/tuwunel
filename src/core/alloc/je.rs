//! jemalloc allocator

use std::{
	alloc::Layout,
	io::Write,
	panic::catch_unwind,
	process::abort,
	sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use jevmalloc::{
	Jemalloc,
	global::hook::{ALLOC, ALLOC_ZEROED},
	stats::print as print_stats,
};
pub use jevmalloc::{arenas::trim, background_thread_enable};
use libc::{STDOUT_FILENO, c_void, write};

use crate::{arrayvec::ArrayVec, utils::BoolExt};

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
	",tcache_max:8192",
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

/// Collects jemalloc's UTF-8 statistics report with the supplied print options.
///
/// Returns `None` if jemalloc produces no report, the report exceeds 1 MiB, or
/// the report contains invalid UTF-8.
#[must_use]
pub fn memory_stats(opts: &str) -> Option<String> {
	const MAX_LENGTH: usize = 1_048_576;

	let mut stats = vec![0; MAX_LENGTH];
	let length = print_stats(opts, &mut stats).ok()?.len();
	if length == 0 {
		return None;
	}

	stats.truncate(length);
	String::from_utf8(stats).ok()
}

/// Exposes jemalloc state controls associated with the calling thread.
///
/// These functions resolve the thread's own arena before applying the
/// operation. They return jevmalloc's control errors unchanged.
pub mod this_thread {
	pub use jevmalloc::thread::this::{decay, set_muzzy_decay};
}
