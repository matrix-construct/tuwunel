//! Block-device, filesystem, and queue-discovery utilities.
//!
//! The helpers inspect filesystem metadata and system block-device information.
//! Discovery covers backing device names, software RAID, multi-queue
//! properties, and the filesystem hosting a path.
//!
//! Discovery reads sysfs under `/sys/dev/block/`, stats the device through
//! `MetadataExt`, and calls `statfs(2)`, which not every target provides.
//! Where an interface is missing these functions report no raid or return an
//! unsupported error, so callers need no condition of their own.

#[cfg(unix)]
use std::{
	ffi::OsStr,
	fs,
	fs::{FileType, read_to_string},
	path::PathBuf,
};
use std::{fmt, path::Path};

#[cfg(unix)]
use itertools::Itertools;
#[cfg(unix)]
use libc::dev_t;

use crate::Result;
#[cfg(unix)]
use crate::{
	result::FlatOk,
	utils::{result::LogDebugErr, string::SplitInfallible},
};

/// Multi-Device (md) i.e. software raid properties.
#[derive(Clone, Debug, Default)]
pub struct MultiDevice {
	/// Type of raid (i.e. `raid1`); None if no raid present or detected.
	pub level: Option<String>,

	/// Number of participating devices.
	pub raid_disks: usize,

	/// The MQ's discovered on the devices; or empty.
	pub md: Vec<MultiQueue>,
}

/// Multi-Queue (mq) characteristics.
#[derive(Clone, Debug, Default)]
pub struct MultiQueue {
	/// Number of requests for the device.
	pub nr_requests: Option<usize>,

	/// Individual queue characteristics.
	pub mq: Vec<Queue>,
}

/// Single-queue characteristics
#[derive(Clone, Debug, Default)]
pub struct Queue {
	/// Queue's indice.
	pub id: usize,

	/// Number of requests for the queue.
	pub nr_tags: Option<usize>,

	/// CPU affinities for the queue.
	pub cpu_list: Vec<usize>,
}

/// Filesystem hosting a path, among those needing special handling.
///
/// Copy-on-Write filesystems interact badly with RocksDB's `fallocate(2)`
/// preallocation. Anything else is reported as absent rather than named.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Filesystem {
	/// Preallocation survives truncation, so a short log pins a whole extent.
	Btrfs,

	/// Preallocation is unimplemented and `fallocate(2)` gives `EOPNOTSUPP`.
	Zfs,
}

impl fmt::Display for Filesystem {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			| Self::Btrfs => "btrfs",
			| Self::Zfs => "ZFS",
		})
	}
}

/// Get properties of a MultiDevice (md) storage system
#[cfg(unix)]
#[must_use]
pub fn md_discover(path: &Path) -> MultiDevice {
	let dev_id = dev_from_path(path)
		.log_debug_err()
		.unwrap_or_default();

	let md_path = block_path(dev_id).join("md/");

	let raid_disks_path = md_path.join("raid_disks");

	let raid_disks: usize = read_to_string(&raid_disks_path)
		.ok()
		.as_deref()
		.map(str::trim)
		.map(str::parse)
		.flat_ok()
		.unwrap_or(0);

	let single_fallback = raid_disks.eq(&0).then(|| block_path(dev_id));

	MultiDevice {
		raid_disks,

		level: read_to_string(md_path.join("level"))
			.ok()
			.as_deref()
			.map(str::trim)
			.map(ToOwned::to_owned),

		md: (0..raid_disks)
			.map(|i| format!("rd{i}/block"))
			.map(|path| md_path.join(&path))
			.filter_map(|ref path| path.canonicalize().ok())
			.map(|mut path| {
				path.pop();
				path
			})
			.chain(single_fallback)
			.map(|path| mq_discover(&path))
			.filter(|mq| !mq.mq.is_empty())
			.collect(),
	}
}

/// Get properties of a MultiDevice (md) storage system.
///
/// Reports no raid, since discovery needs sysfs, which this platform does
/// not have.
#[cfg(not(unix))]
#[must_use]
pub fn md_discover(_path: &Path) -> MultiDevice { MultiDevice::default() }

/// Get properties of a MultiQueue within a MultiDevice.
#[cfg(unix)]
#[must_use]
fn mq_discover(path: &Path) -> MultiQueue {
	let mq_path = path.join("mq/");

	let nr_requests_path = path.join("queue/nr_requests");

	MultiQueue {
		nr_requests: read_to_string(&nr_requests_path)
			.ok()
			.as_deref()
			.map(str::trim)
			.map(str::parse)
			.flat_ok(),

		mq: fs::read_dir(&mq_path)
			.into_iter()
			.flat_map(IntoIterator::into_iter)
			.filter_map(Result::ok)
			.filter(|entry| {
				entry
					.file_type()
					.as_ref()
					.is_ok_and(FileType::is_dir)
			})
			.map(|dir| queue_discover(&dir.path()))
			.sorted_by_key(|mq| mq.id)
			.collect::<Vec<_>>(),
	}
}

/// Get properties of a Queue within a MultiQueue.
#[cfg(unix)]
fn queue_discover(dir: &Path) -> Queue {
	let queue_id = dir.file_name();

	let nr_tags_path = dir.join("nr_tags");

	let cpu_list_path = dir.join("cpu_list");

	Queue {
		id: queue_id
			.and_then(OsStr::to_str)
			.map(str::parse)
			.flat_ok()
			.expect("queue has some numerical identifier"),

		nr_tags: read_to_string(&nr_tags_path)
			.ok()
			.as_deref()
			.map(str::trim)
			.map(str::parse)
			.flat_ok(),

		cpu_list: read_to_string(&cpu_list_path)
			.iter()
			.flat_map(|list| list.trim().split(','))
			.map(str::trim)
			.map(str::parse)
			.filter_map(Result::ok)
			.collect(),
	}
}

/// Get the name of the block device on which Path is mounted.
#[cfg(unix)]
pub fn name_from_path(path: &Path) -> Result<String> {
	use std::io::{Error, ErrorKind::NotFound};

	let (major, minor) = dev_from_path(path)?;
	let path = block_path((major, minor)).join("uevent");
	read_to_string(path)
		.iter()
		.map(String::as_str)
		.flat_map(str::lines)
		.map(|line| line.split_once_infallible("="))
		.find_map(|(key, val)| (key == "DEVNAME").then_some(val))
		.ok_or_else(|| Error::new(NotFound, "DEVNAME not found."))
		.map_err(Into::into)
		.map(Into::into)
}

/// Get the name of the block device on which Path is mounted.
///
/// Naming the device requires sysfs, so this always returns an unsupported
/// error.
#[cfg(not(unix))]
pub fn name_from_path(_path: &Path) -> Result<String> {
	use std::io::{Error, ErrorKind::Unsupported};

	Err(Error::new(Unsupported, "Block device discovery requires sysfs.").into())
}

/// Get the filesystem on which Path is mounted, when it needs special
/// handling.
///
/// Linux reports a superblock magic rather than a name, and neither libc nor
/// nix carries one for out-of-tree OpenZFS. Any other filesystem reports
/// `None`.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn filesystem_from_path(path: &Path) -> Result<Option<Filesystem>> {
	use nix::sys::statfs::{BTRFS_SUPER_MAGIC, FsType, statfs};

	// <https://github.com/openzfs/zfs/blob/zfs-2.4.3/include/sys/fs/zfs.h>
	const ZFS_SUPER_MAGIC: FsType = FsType(0x2FC1_2FC1);

	Ok(match statfs(path)?.filesystem_type() {
		| BTRFS_SUPER_MAGIC => Some(Filesystem::Btrfs),
		| ZFS_SUPER_MAGIC => Some(Filesystem::Zfs),
		| _ => None,
	})
}

/// Get the filesystem on which Path is mounted, when it needs special
/// handling.
///
/// The BSDs and macOS report a type name rather than a superblock magic. Any
/// other filesystem reports `None`.
#[cfg(any(
	target_os = "freebsd",
	target_os = "dragonfly",
	target_os = "openbsd",
	target_vendor = "apple"
))]
pub fn filesystem_from_path(path: &Path) -> Result<Option<Filesystem>> {
	use nix::sys::statfs::statfs;

	Ok(match statfs(path)?.filesystem_type_name() {
		| "btrfs" => Some(Filesystem::Btrfs),
		| "zfs" => Some(Filesystem::Zfs),
		| _ => None,
	})
}

/// Get the filesystem on which Path is mounted, when it needs special
/// handling.
///
/// Naming the filesystem requires `statfs(2)`, so this always returns an
/// unsupported error.
#[cfg(not(any(
	target_os = "linux",
	target_os = "android",
	target_os = "freebsd",
	target_os = "dragonfly",
	target_os = "openbsd",
	target_vendor = "apple"
)))]
pub fn filesystem_from_path(_path: &Path) -> Result<Option<Filesystem>> {
	use std::io::{Error, ErrorKind::Unsupported};

	Err(Error::new(Unsupported, "Filesystem discovery requires statfs.").into())
}

/// Get the (major, minor) of the block device on which Path is mounted.
#[cfg(unix)]
fn dev_from_path(path: &Path) -> Result<(dev_t, dev_t)> {
	use std::os::unix::fs::MetadataExt;

	let stat = fs::metadata(path)?;

	// Metadata::dev() is u64 on every unix; dev_t itself is not, so the
	// conversions below differ per platform.
	#[cfg(target_os = "linux")]
	let dev_id = stat.dev();

	#[cfg(not(target_os = "linux"))]
	let dev_id = stat.dev().try_into()?;

	let (major, minor) = (libc::major(dev_id), libc::minor(dev_id));

	#[cfg(target_os = "linux")]
	let (major, minor) = (major.into(), minor.into());

	#[cfg(target_os = "android")]
	let (major, minor) = (major.try_into()?, minor.try_into()?);

	#[cfg(not(any(
		target_os = "linux",
		target_os = "android",
		target_vendor = "apple"
	)))]
	let (major, minor) = (major.try_into()?, minor.try_into()?);

	Ok((major, minor))
}

#[cfg(unix)]
fn block_path((major, minor): (dev_t, dev_t)) -> PathBuf {
	format!("/sys/dev/block/{major}:{minor}/").into()
}
