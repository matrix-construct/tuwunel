#[cfg(unix)]
use nix::sys::resource::{Resource, getrlimit};
#[cfg(unix)]
use nix::unistd::{SysconfVar, sysconf};

use crate::Result;
#[cfg(unix)]
use crate::{apply, debug, utils::math::ExpectInto};

#[cfg(unix)]
/// Raises the soft file descriptor limit to the current hard limit.
///
/// RocksDB and concurrent federation connections can exceed the common soft
/// limit of 1,024 during startup. Systemd commonly provides a hard limit of
/// 524,288.
///
/// * <https://www.freedesktop.org/software/systemd/man/systemd.exec.html#id-1.12.2.1.17.6>
/// * <https://github.com/systemd/systemd/commit/0abf94923b4a95a7d89bc526efc84e7ca2b71741>
pub fn maximize_fd_limit() -> Result {
	use nix::sys::resource::setrlimit;

	let (soft_limit, hard_limit) = max_file_descriptors()?;
	if soft_limit < hard_limit {
		let new_limit = hard_limit.try_into()?;
		setrlimit(Resource::RLIMIT_NOFILE, new_limit, new_limit)?;
		assert_eq!((hard_limit, hard_limit), max_file_descriptors()?, "getrlimit != setrlimit");
		debug!(to = hard_limit, from = soft_limit, "Raised RLIMIT_NOFILE");
	}

	Ok(())
}

#[cfg(not(unix))]
pub fn maximize_fd_limit() -> Result { Ok(()) }

#[cfg(all(unix, not(target_os = "macos")))]
/// Raises the soft thread limit to the current hard limit.
///
/// Some distributions default to about 1,024 threads, which can constrain hosts
/// with 32 or more cores. Thread limits are otherwise reached less often than
/// file descriptor limits.
pub fn maximize_thread_limit() -> Result {
	use nix::sys::resource::setrlimit;

	let (soft_limit, hard_limit) = max_threads()?;
	if soft_limit < hard_limit {
		let new_limit = hard_limit.try_into()?;
		setrlimit(Resource::RLIMIT_NPROC, new_limit, new_limit)?;
		assert_eq!((hard_limit, hard_limit), max_threads()?, "getrlimit != setrlimit");
		debug!(to = hard_limit, from = soft_limit, "Raised RLIMIT_NPROC");
	}

	Ok(())
}

#[cfg(any(not(unix), target_os = "macos"))]
pub fn maximize_thread_limit() -> Result { Ok(()) }

/// Returns the soft and hard file-descriptor limits.
///
/// The tuple is ordered as the current soft limit followed by the maximum hard
/// limit. Values come from `RLIMIT_NOFILE`.
///
/// # Panics
///
/// Panics when either platform limit cannot be represented as `usize`.
#[cfg(unix)]
#[inline]
pub fn max_file_descriptors() -> Result<(usize, usize)> {
	getrlimit(Resource::RLIMIT_NOFILE)
		.map(apply!(2, ExpectInto::expect_into))
		.map_err(Into::into)
}

/// Returns sentinel file-descriptor limits on unsupported platforms.
///
/// Both tuple elements are `usize::MAX`, representing no known finite limit.
/// No operating-system query is performed.
#[cfg(not(unix))]
#[inline]
pub fn max_file_descriptors() -> Result<(usize, usize)> { Ok((usize::MAX, usize::MAX)) }

/// Returns the soft and hard process stack-size limits.
///
/// The tuple is ordered as the current soft limit followed by the maximum hard
/// limit. Values come from `RLIMIT_STACK`.
///
/// # Panics
///
/// Panics when either platform limit cannot be represented as `usize`.
#[cfg(unix)]
#[inline]
pub fn max_stack_size() -> Result<(usize, usize)> {
	getrlimit(Resource::RLIMIT_STACK)
		.map(apply!(2, ExpectInto::expect_into))
		.map_err(Into::into)
}

/// Returns sentinel stack-size limits on unsupported platforms.
///
/// Both tuple elements are `usize::MAX`, representing no known finite limit.
/// No operating-system query is performed.
#[cfg(not(unix))]
#[inline]
pub fn max_stack_size() -> Result<(usize, usize)> { Ok((usize::MAX, usize::MAX)) }

/// Returns the soft and hard locked-memory limits.
///
/// The tuple is ordered as the current soft limit followed by the maximum hard
/// limit. Values come from `RLIMIT_MEMLOCK`.
///
/// # Panics
///
/// Panics when either platform limit cannot be represented as `usize`.
#[cfg(all(unix, not(target_os = "macos")))]
#[inline]
pub fn max_memory_locked() -> Result<(usize, usize)> {
	getrlimit(Resource::RLIMIT_MEMLOCK)
		.map(apply!(2, ExpectInto::expect_into))
		.map_err(Into::into)
}

/// Returns sentinel locked-memory limits on unsupported platforms.
///
/// Both tuple elements are zero, the module's unsupported-platform sentinel.
/// No operating-system query is performed.
#[cfg(any(not(unix), target_os = "macos"))]
#[inline]
pub fn max_memory_locked() -> Result<(usize, usize)> { Ok((usize::MIN, usize::MIN)) }

/// Returns the soft and hard per-user process-count limits.
///
/// The tuple is ordered as the current soft limit followed by the maximum hard
/// limit. Values come from `RLIMIT_NPROC`; on Linux, it counts extant threads
/// for the caller's real user ID.
///
/// # Panics
///
/// Panics when either platform limit cannot be represented as `usize`.
#[cfg(all(unix, not(target_os = "macos")))]
#[inline]
pub fn max_threads() -> Result<(usize, usize)> {
	getrlimit(Resource::RLIMIT_NPROC)
		.map(apply!(2, ExpectInto::expect_into))
		.map_err(Into::into)
}

/// Returns sentinel thread limits on unsupported platforms.
///
/// Both tuple elements are `usize::MAX`, representing no known finite limit.
/// No operating-system query is performed.
#[cfg(any(not(unix), target_os = "macos"))]
#[inline]
pub fn max_threads() -> Result<(usize, usize)> { Ok((usize::MAX, usize::MAX)) }

#[cfg(unix)]
/// Get the system's page size in bytes.
#[inline]
pub fn page_size() -> Result<usize> {
	sysconf(SysconfVar::PAGE_SIZE)?
		.unwrap_or(-1)
		.try_into()
		.map_err(Into::into)
}

#[cfg(not(unix))]
/// Get the system's page size in bytes.
#[inline]
pub fn page_size() -> Result<usize> { Ok(4096) }
