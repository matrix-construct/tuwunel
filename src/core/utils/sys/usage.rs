#[cfg(unix)]
use nix::sys::resource::{Usage as NixUsage, UsageWho, getrusage};

use crate::{Result, expected};

/// Resource usage wrapper. On Unix this is the nix `Usage` struct populated
/// by `getrusage()`. On Windows (and any platform where `getrusage` is
/// unavailable) this is a zero-field Debug stub so that tracing macros like
/// `?resource_usage` still work.
#[cfg(unix)]
pub type Usage = NixUsage;

#[cfg(not(unix))]
#[derive(Debug, Default, Clone, Copy)]
pub struct Usage;

pub fn virt() -> Result<usize> {
	Ok(statm_bytes()?
		.next()
		.expect("incomplete statm contents"))
}

pub fn res() -> Result<usize> {
	Ok(statm_bytes()?
		.nth(1)
		.expect("incomplete statm contents"))
}

pub fn shm() -> Result<usize> {
	Ok(statm_bytes()?
		.nth(2)
		.expect("incomplete statm contents"))
}

pub fn code() -> Result<usize> {
	Ok(statm_bytes()?
		.nth(3)
		.expect("incomplete statm contents"))
}

pub fn data() -> Result<usize> {
	Ok(statm_bytes()?
		.nth(5)
		.expect("incomplete statm contents"))
}

#[inline]
pub fn statm_bytes() -> Result<impl Iterator<Item = usize>> {
	let page_size = super::page_size()?;

	Ok(statm()?.map(move |pages| expected!(pages * page_size)))
}

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

#[cfg(not(target_os = "linux"))]
#[inline]
pub fn statm() -> Result<impl Iterator<Item = usize>> { Ok([0, 0, 0, 0, 0, 0].into_iter()) }

#[cfg(unix)]
pub fn usage() -> Result<Usage> { getrusage(UsageWho::RUSAGE_SELF).map_err(Into::into) }

#[cfg(not(unix))]
pub fn usage() -> Result<Usage> { Ok(Usage) }

/// Per-thread resource usage. On Linux/FreeBSD/OpenBSD, `RUSAGE_THREAD` is
/// available. On other Unix platforms (macOS, etc.) the thread variant does
/// not exist, so we fall back to the process-wide `getrusage(RUSAGE_SELF)`
/// which returns a non-zero, well-defined `Usage` value rather than
/// relying on `Usage::default()` (which nix does not implement).
#[cfg(any(
	target_os = "linux",
	target_os = "freebsd",
	target_os = "openbsd"
))]
pub fn thread_usage() -> Result<Usage> { getrusage(UsageWho::RUSAGE_THREAD).map_err(Into::into) }

#[cfg(all(
	unix,
	not(any(
		target_os = "linux",
		target_os = "freebsd",
		target_os = "openbsd"
	))
))]
pub fn thread_usage() -> Result<Usage> { getrusage(UsageWho::RUSAGE_SELF).map_err(Into::into) }

#[cfg(not(unix))]
pub fn thread_usage() -> Result<Usage> { Ok(Usage) }
