pub mod compute;
pub mod limits;
pub mod storage;
pub mod usage;

use std::path::PathBuf;

pub use self::{
	compute::available_parallelism,
	limits::*,
	usage::{Usage, statm, thread_usage, usage},
};
use crate::{Result, at};

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

#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
pub enum SocketFamily {
	Inet,
	Unix,
}

#[cfg(unix)]
pub fn get_socket_family(fd: i32) -> Result<SocketFamily> {
	use nix::sys::socket::{AddressFamily, SockaddrLike, SockaddrStorage};

	use crate::{Err, err};

	let sockname: SockaddrStorage = nix::sys::socket::getsockname(fd)?;

	let family = sockname
		.family()
		.ok_or_else(|| err!("Invalid socket"))?;

	match family {
		| AddressFamily::Inet | AddressFamily::Inet6 => Ok(SocketFamily::Inet),
		| AddressFamily::Unix => Ok(SocketFamily::Unix),
		| _ => Err!("Unknown socket family: {family:?}"),
	}
}
