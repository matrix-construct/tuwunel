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

/// Returns resource usage for the current thread when the platform supports it.
///
/// Linux, FreeBSD, and OpenBSD report thread-specific usage. Other Unix
/// platforms fall back to process-wide usage. Platforms without `getrusage()`
/// return the zero-field [`Usage`] stub.
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
