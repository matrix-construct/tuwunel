pub mod compute;
pub mod storage;

use std::path::PathBuf;

pub use compute::available_parallelism;

use crate::{Result, at, debug};

/// This is needed for opening lots of file descriptors, which tends to
/// happen more often when using RocksDB and making lots of federation
/// connections at startup. The soft limit is usually 1024, and the hard
/// limit is usually 512000; I've personally seen it hit >2000.
///
/// * <https://www.freedesktop.org/software/systemd/man/systemd.exec.html#id-1.12.2.1.17.6>
/// * <https://github.com/systemd/systemd/commit/0abf94923b4a95a7d89bc526efc84e7ca2b71741>
#[cfg(unix)]
pub fn maximize_fd_limit() -> Result<(), nix::errno::Errno> {
	use nix::sys::resource::{Resource::RLIMIT_NOFILE as NOFILE, getrlimit, setrlimit};

	let (soft_limit, hard_limit) = getrlimit(NOFILE)?;
	if soft_limit < hard_limit {
		setrlimit(NOFILE, hard_limit, hard_limit)?;
		assert_eq!((hard_limit, hard_limit), getrlimit(NOFILE)?, "getrlimit != setrlimit");
		debug!(to = hard_limit, from = soft_limit, "Raised RLIMIT_NOFILE",);
	}

	Ok(())
}

/// Return a possibly corrected std::env::current_exe() even if the path is
/// marked deleted.
pub fn current_exe() -> Result<PathBuf> {
	let exe = std::env::current_exe()?;
	match exe.to_str() {
		| None => Ok(exe),
		| Some(str) => Ok(str
			.strip_suffix(" (deleted)")
			.map(PathBuf::from)
			.unwrap_or(exe)),
	}
}

/// Determine if the server's executable was removed or replaced. This is a
/// specific check; useful for successful restarts. May not be available or
/// accurate on all platforms; defaults to false.
#[must_use]
pub fn current_exe_deleted() -> bool {
	std::env::current_exe().is_ok_and(|exe| {
		exe.to_str()
			.is_some_and(|exe| exe.ends_with(" (deleted)"))
	})
}

/// Parse the `KEY=VALUE` contents of a `uevent` file searching for `key` and
/// returning the `value`.
#[inline]
#[must_use]
pub fn uevent_find<'a>(uevent: &'a str, key: &'a str) -> Option<&'a str> {
	uevent
		.lines()
		.filter_map(|line| line.split_once('='))
		.find(|&(key_, _)| key.eq(key_))
		.map(at!(1))
}
