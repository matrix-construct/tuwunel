//! System utilities related to devices/peripherals

use std::{
	ffi::OsStr,
	fs,
	fs::{FileType, read_to_string},
	iter::IntoIterator,
	path::{Path, PathBuf},
};

use itertools::Itertools;
use libc::dev_t;

use crate::{
	Result,
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

/// Get properties of a MultiDevice (md) storage system
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

/// Get properties of a MultiQueue within a MultiDevice.
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

/// Get the (major, minor) of the block device on which Path is mounted.
#[expect(
	clippy::useless_conversion,
	clippy::unnecessary_fallible_conversions
)]
fn dev_from_path(path: &Path) -> Result<(dev_t, dev_t)> {
	#[cfg(target_family = "unix")]
	use std::os::unix::fs::MetadataExt;

	let stat = fs::metadata(path)?;
	let dev_id = stat.dev().try_into()?;
	let (major, minor) = (libc::major(dev_id), libc::minor(dev_id));

	Ok((major.try_into()?, minor.try_into()?))
}

fn block_path((major, minor): (dev_t, dev_t)) -> PathBuf {
	format!("/sys/dev/block/{major}:{minor}/").into()
}
