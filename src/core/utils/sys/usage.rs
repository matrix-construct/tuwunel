#[cfg(unix)]
use nix::sys::resource::{Usage as NixUsage, UsageWho, getrusage};

use crate::{Result, expected};

/// Platform representation of process resource usage.
///
/// On Unix this aliases nix's `Usage`, populated by `getrusage()`. Platforms
/// without `getrusage()` use a zero-field `Debug` stub so tracing fields such
/// as `?resource_usage` remain portable.
#[cfg(unix)]
pub type Usage = NixUsage;

/// Portable resource-usage stub for platforms without `getrusage()`.
///
/// The zero-field value keeps tracing and call sites portable. It contains no
/// process or thread measurements.
#[cfg(not(unix))]
#[derive(Debug, Default, Clone, Copy)]
pub struct Usage;

/// Returns the process's virtual memory size in bytes, or zero outside Linux.
///
/// The value is the first field of Linux `/proc/self/statm`, scaled by the
/// system page size. It is sampled separately from the other memory fields.
///
/// # Panics
///
/// On Linux, panics if `statm` is malformed, exceeds parser capacity, or lacks
/// the requested field. Panics if converting that page count overflows `usize`
/// bytes.
pub fn virt() -> Result<usize> {
	Ok(statm_bytes()?
		.next()
		.expect("incomplete statm contents"))
}

/// Returns the process's resident memory size in bytes, or zero outside Linux.
///
/// The value is the second field of Linux `/proc/self/statm`, scaled by the
/// system page size. It is sampled separately from the other memory fields.
///
/// # Panics
///
/// On Linux, panics if `statm` is malformed, exceeds parser capacity, or lacks
/// the requested field. Panics if converting that page count overflows `usize`
/// bytes.
pub fn res() -> Result<usize> {
	Ok(statm_bytes()?
		.nth(1)
		.expect("incomplete statm contents"))
}

/// Returns shared resident memory size in bytes, or zero outside Linux.
///
/// The value is the third field of Linux `/proc/self/statm`, scaled by the
/// system page size. It is sampled separately from the other memory fields.
///
/// # Panics
///
/// On Linux, panics if `statm` is malformed, exceeds parser capacity, or lacks
/// the requested field. Panics if converting that page count overflows `usize`
/// bytes.
pub fn shm() -> Result<usize> {
	Ok(statm_bytes()?
		.nth(2)
		.expect("incomplete statm contents"))
}

/// Returns the process's resident code size in bytes, or zero outside Linux.
///
/// The value is the fourth field of Linux `/proc/self/statm`, scaled by the
/// system page size. It represents the resident text segment.
///
/// # Panics
///
/// On Linux, panics if `statm` is malformed, exceeds parser capacity, or lacks
/// the requested field. Panics if converting that page count overflows `usize`
/// bytes.
pub fn code() -> Result<usize> {
	Ok(statm_bytes()?
		.nth(3)
		.expect("incomplete statm contents"))
}

/// Returns the process's data and stack size in bytes, or zero outside Linux.
///
/// The value is the sixth field of Linux `/proc/self/statm`, scaled by the
/// system page size. It is sampled separately from the other memory fields.
///
/// # Panics
///
/// On Linux, panics if `statm` is malformed, exceeds parser capacity, or lacks
/// the requested field. Panics if converting that page count overflows `usize`
/// bytes.
pub fn data() -> Result<usize> {
	Ok(statm_bytes()?
		.nth(5)
		.expect("incomplete statm contents"))
}

/// Returns the `statm` page counts converted to bytes.
///
/// Each page count is multiplied by the system page size as the iterator is
/// consumed. The field order remains the Linux `statm` order.
///
/// # Panics
///
/// On Linux, panics before returning if `statm` is malformed or exceeds parser
/// capacity. Panics while iterating if converting a page count overflows
/// `usize` bytes.
#[inline]
pub fn statm_bytes() -> Result<impl Iterator<Item = usize>> {
	let page_size = super::page_size()?;

	Ok(statm()?.map(move |pages| expected!(pages * page_size)))
}

/// Returns process memory statistics as page counts.
///
/// Linux parses a snapshot of `/proc/self/statm` in kernel field order. The
/// returned iterator owns the parsed counts.
///
/// # Panics
///
/// Panics when `statm` contains non-UTF-8 or non-integer data, or more than 12
/// fields.
#[cfg(target_os = "linux")]
#[inline]
pub fn statm() -> Result<impl Iterator<Item = usize>> {
	use std::{fs::File, io::Read, str};

	use crate::{Error, arrayvec::ArrayVec};

	File::open("/proc/self/statm")
		.map_err(Error::from)
		.and_then(|mut fp| {
			let mut buf = [0; 96];
			let len = fp.read(&mut buf)?;
			let vals = str::from_utf8(&buf[0..len])
				.expect("non-utf8 content in statm")
				.split_ascii_whitespace()
				.map(|val| {
					val.parse()
						.expect("non-integer value in statm contents")
				})
				.collect::<ArrayVec<usize, 12>>();

			Ok(vals.into_iter())
		})
}

/// Returns process memory statistics as page counts.
///
/// Non-Linux platforms return six zero counts in Linux `statm` field order. No
/// operating-system query is performed.
#[cfg(not(target_os = "linux"))]
#[inline]
pub fn statm() -> Result<impl Iterator<Item = usize>> { Ok([0, 0, 0, 0, 0, 0].into_iter()) }

/// Returns resource usage for the current process.
///
/// Unix platforms obtain a fresh `RUSAGE_SELF` snapshot from `getrusage()`.
/// The measurement includes resources consumed by the process at call time.
#[cfg(unix)]
pub fn usage() -> Result<Usage> { getrusage(UsageWho::RUSAGE_SELF).map_err(Into::into) }

/// Returns the portable process resource-usage stub.
///
/// Platforms without `getrusage()` expose no measurements through this API.
/// The returned zero-field value keeps callers platform-independent.
#[cfg(not(unix))]
pub fn usage() -> Result<Usage> { Ok(Usage) }

#[cfg(any(
	target_os = "linux",
	target_os = "freebsd",
	target_os = "openbsd"
))]
/// Returns resource usage for the current thread when the platform supports it.
///
/// Linux, FreeBSD, and OpenBSD report thread-specific usage. Other Unix
/// platforms fall back to process-wide usage. Platforms without `getrusage()`
/// return the zero-field [`Usage`] stub.
pub fn thread_usage() -> Result<Usage> { getrusage(UsageWho::RUSAGE_THREAD).map_err(Into::into) }

#[cfg(all(
	unix,
	not(any(
		target_os = "linux",
		target_os = "freebsd",
		target_os = "openbsd"
	))
))]
/// Returns resource usage for the current thread when the platform supports it.
///
/// Linux, FreeBSD, and OpenBSD report thread-specific usage. Other Unix
/// platforms fall back to process-wide usage. Platforms without `getrusage()`
/// return the zero-field [`Usage`] stub.
pub fn thread_usage() -> Result<Usage> { getrusage(UsageWho::RUSAGE_SELF).map_err(Into::into) }

#[cfg(not(unix))]
/// Returns resource usage for the current thread when the platform supports it.
///
/// Linux, FreeBSD, and OpenBSD report thread-specific usage. Other Unix
/// platforms fall back to process-wide usage. Platforms without `getrusage()`
/// return the zero-field [`Usage`] stub.
pub fn thread_usage() -> Result<Usage> { Ok(Usage) }
