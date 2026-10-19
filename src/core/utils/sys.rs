//! Operating-system resource and platform-integration utilities.
//!
//! Submodules expose compute, resource-limit, storage, and usage information.
//! Top-level helpers normalize executable paths, parse device metadata, and
//! classify socket file descriptors on Unix.

pub mod compute;

pub mod limits;

pub mod storage;

pub mod usage;

#[cfg(all(test, any(unix, windows)))]
mod tests;

#[cfg(unix)]
use std::os::fd::AsFd;
use std::path::PathBuf;

#[cfg(unix)]
use nix::{
	errno::Errno,
	sys::socket::{getsockopt, sockopt::Ipv6V6Only},
};

pub use self::{
	compute::available_parallelism,
	limits::*,
	usage::{Usage, statm, thread_usage, usage},
};
use crate::{Result, at};

/// Returns the current executable path without a trailing deletion marker.
///
/// The literal ` (deleted)` suffix is removed when the path is valid UTF-8.
/// Other paths remain unchanged, and executable lookup errors are propagated.
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

/// Reports whether the current executable path carries a deletion marker.
///
/// A trailing ` (deleted)` suffix can indicate that the executable was removed
/// or replaced. Lookup failures and non-UTF-8 paths return `false`.
#[must_use]
pub fn current_exe_deleted() -> bool {
	std::env::current_exe().is_ok_and(|exe| {
		exe.to_str()
			.is_some_and(|exe| exe.ends_with(" (deleted)"))
	})
}

/// Searches newline-delimited `KEY=VALUE` text for a key.
///
/// Lines without `=` are ignored, and the first exact key match is returned.
/// The borrowed value contains everything after the first `=`, including any
/// additional separators.
#[inline]
#[must_use]
pub fn uevent_find<'a>(uevent: &'a str, key: &'a str) -> Option<&'a str> {
	uevent
		.lines()
		.filter_map(|line| line.split_once('='))
		.find(|&(key_, _)| key.eq(key_))
		.map(at!(1))
}

/// Classifies the socket address families recognized by the server.
///
/// IPv4 and IPv6 addresses share the Internet variant, while Unix-domain
/// addresses use the local variant. Other address families are rejected by
/// [`get_socket_family`].
#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
pub enum SocketFamily {
	/// An IPv4 or IPv6 Internet socket.
	///
	/// Both Internet address families map to this variant.
	Inet,

	/// A Unix-domain socket.
	///
	/// Every recognized local socket address maps to this variant.
	Unix,
}

/// Determines the address family of an open socket file descriptor on Unix.
///
/// IPv4 and IPv6 descriptors return [`SocketFamily::Inet`], and Unix-domain
/// descriptors return [`SocketFamily::Unix`]. Socket inspection failures,
/// missing families, and unsupported families are returned as errors.
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

/// Whether an IPv6 socket serves IPv6 traffic alone.
///
/// A dual-stack socket answers on the IPv4-mapped range too, so an unspecified
/// address of one binds both families. The option does not exist on other
/// families, which are reported as unrestricted.
#[cfg(unix)]
pub fn is_ipv6_only<F: AsFd>(socket: &F) -> Result<bool> {
	getsockopt(socket, Ipv6V6Only).or_else(|e| match e {
		| Errno::ENOPROTOOPT => Ok(false),
		| _ => Err(e.into()),
	})
}
