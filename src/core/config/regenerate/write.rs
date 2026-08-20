#[cfg(unix)]
use std::fs::Permissions;
#[cfg(unix)]
use std::os::unix::{
	fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _},
	io::AsRawFd as _,
};
#[cfg(target_os = "macos")]
use std::ptr::null_mut;
use std::{
	ffi::OsString,
	fmt::Display,
	fs::{File, Metadata, OpenOptions, hard_link, remove_file, symlink_metadata},
	io::{
		Error as IoError, ErrorKind, Read as _, Result as IoResult, Seek as _, SeekFrom,
		Write as _, copy,
	},
	path::{Path, PathBuf},
	process::id,
	sync::atomic::{AtomicU64, Ordering},
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::{
	ffi::{CStr, CString},
	os::unix::ffi::OsStrExt as _,
};

#[cfg(target_os = "linux")]
use libc::{AT_FDCWD, RENAME_EXCHANGE, SYS_renameat2, syscall};
#[cfg(target_os = "macos")]
use libc::{COPYFILE_ACL, COPYFILE_XATTR, RENAME_SWAP, fcopyfile, renamex_np};
#[cfg(unix)]
use libc::{O_CLOEXEC, O_NOFOLLOW, O_NONBLOCK, fchown};

#[cfg(target_os = "macos")]
use crate::warn;
use crate::{Err, Error, Result, debug_warn, err};

type Origin<'a> = (&'a File, &'a Metadata);

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TempGuard {
	path: PathBuf,
	armed: bool,
}

pub(super) fn write_atomic(path: &Path, content: &[u8], force: bool) -> Result {
	write_atomic_inner(path, content, force, || {})
}

#[cfg(test)]
pub(super) fn write_atomic_with_precommit(
	path: &Path,
	content: &[u8],
	force: bool,
	before_commit: impl FnOnce(),
) -> Result {
	write_atomic_inner(path, content, force, before_commit)
}

fn write_atomic_inner(
	path: &Path,
	content: &[u8],
	force: bool,
	before_commit: impl FnOnce(),
) -> Result {
	let parent = path
		.parent()
		.filter(|parent| !parent.as_os_str().is_empty())
		.unwrap_or_else(|| Path::new("."));

	let metadata = match symlink_metadata(path) {
		| Err(error) if error.kind() == ErrorKind::NotFound => None,
		| Ok(metadata) => Some(metadata),
		| Err(error) => return Err(fs_error(&error, "inspect output", path)),
	};

	match metadata {
		| Some(metadata) if is_nonregular(&metadata) => {
			Err!("Output is not a regular file: {}.", path.display())
		},
		| Some(_) if !force => Err!("Output already exists: {}.", path.display()),
		| Some(_) => write_replacement(parent, path, content, before_commit),
		| None => write_new(parent, path, content, before_commit),
	}
}

fn write_new(parent: &Path, path: &Path, content: &[u8], before_commit: impl FnOnce()) -> Result {
	let mut output = create_synced_temp(parent, path, content, None)?;

	before_commit();
	install_noreplace(&output.path, path, "output")?;

	cleanup_after_commit(&mut output);
	sync_after_commit(parent, path);

	Ok(())
}

fn write_replacement(
	parent: &Path,
	path: &Path,
	content: &[u8],
	before_commit: impl FnOnce(),
) -> Result {
	let backup = backup_path(path);

	if entry_exists(&backup)? {
		return Err!("Backup already exists: {}.", backup.display());
	}

	let (mut source, metadata) = open_target(path)?;
	let mut output = create_synced_temp(parent, path, content, Some((&source, &metadata)))?;
	let (mut backup_guard, mut backup_file) =
		copy_backup(parent, &backup, &mut source, &metadata)?;

	install_noreplace(&backup_guard.path, &backup, "backup")?;
	sync_parent(parent)
		.map_err(|error| {
			err!("Failed to synchronize staged backup `{}`: {error}", backup.display())
		})
		.map_err(|error| discard_backup(&backup, parent, error))?;

	before_commit();
	exchange(&output.path, path).map_err(|error| discard_backup(&backup, parent, error))?;
	validate_displaced(
		&output.path,
		&metadata,
		&mut source,
		&mut backup_file,
		&backup_guard.path,
	)
	.map_err(|error| {
		rollback_replacement(&mut output, &mut backup_guard, path, &backup, error)
	})?;

	preserve_backup_attributes(&source, &backup_file, &backup);
	cleanup_after_commit(&mut output);
	cleanup_after_commit(&mut backup_guard);
	sync_after_commit(parent, path);

	Ok(())
}

fn create_synced_temp(
	parent: &Path,
	target: &Path,
	content: &[u8],
	origin: Option<Origin<'_>>,
) -> Result<TempGuard> {
	let (path, mut file) = create_temp(parent, target)?;
	let guard = TempGuard { path, armed: true };

	file.write_all(content)
		.map_err(|error| fs_error(&error, "write temporary output", &guard.path))?;

	set_output_metadata(&file, origin.map(|(_, metadata)| metadata), &guard.path)?;

	if let Some((source, _)) = origin {
		copy_security_attributes(source, &file, &guard.path)?;
	}

	file.sync_all()
		.map_err(|error| fs_error(&error, "synchronize temporary output", &guard.path))?;

	Ok(guard)
}

fn copy_backup(
	parent: &Path,
	backup: &Path,
	source: &mut File,
	metadata: &Metadata,
) -> Result<(TempGuard, File)> {
	let (path, mut file) = create_temp(parent, backup)?;
	let guard = TempGuard { path, armed: true };

	copy(source, &mut file)
		.map_err(|error| fs_pair_error(&error, "copy output", backup, &guard.path))?;

	set_output_metadata(&file, Some(metadata), &guard.path)?;

	file.sync_all()
		.map_err(|error| fs_error(&error, "synchronize temporary backup", &guard.path))?;

	Ok((guard, file))
}

fn open_target(path: &Path) -> Result<(File, Metadata)> {
	let mut options = OpenOptions::new();

	options.read(true);

	#[cfg(unix)]
	options.custom_flags(O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK);

	let file = options
		.open(path)
		.map_err(|error| fs_error(&error, "open output", path))?;

	let metadata = file
		.metadata()
		.map_err(|error| fs_error(&error, "inspect opened output", path))?;

	if is_nonregular(&metadata) {
		return Err!("Output is not a regular file: {}.", path.display());
	}

	Ok((file, metadata))
}

fn validate_displaced(
	displaced: &Path,
	metadata: &Metadata,
	source: &mut File,
	backup: &mut File,
	backup_path: &Path,
) -> Result {
	let displaced_metadata = symlink_metadata(displaced)
		.map_err(|error| fs_error(&error, "inspect displaced output", displaced))?;

	if !same_file(metadata, &displaced_metadata) {
		return Err!("Output changed while its replacement was being prepared.");
	}

	if !same_contents(source, backup, displaced, backup_path)? {
		return Err!("Output contents changed while its replacement was being prepared.");
	}

	Ok(())
}

fn same_contents(
	left: &mut File,
	right: &mut File,
	left_path: &Path,
	right_path: &Path,
) -> Result<bool> {
	left.seek(SeekFrom::Start(0))
		.map_err(|error| fs_error(&error, "rewind displaced output", left_path))?;

	right
		.seek(SeekFrom::Start(0))
		.map_err(|error| fs_error(&error, "rewind temporary backup", right_path))?;

	let mut left_buf = [0_u8; 4096];
	let mut right_buf = [0_u8; 4096];

	loop {
		let left_len = left
			.read(&mut left_buf)
			.map_err(|error| fs_error(&error, "read displaced output", left_path))?;

		let right_len = right
			.read(&mut right_buf)
			.map_err(|error| fs_error(&error, "read temporary backup", right_path))?;

		if left_len != right_len {
			return Ok(false);
		}

		if left_buf[..left_len] != right_buf[..left_len] {
			return Ok(false);
		}

		if left_len == 0 {
			return Ok(true);
		}
	}
}

fn rollback_replacement(
	output: &mut TempGuard,
	backup_guard: &mut TempGuard,
	path: &Path,
	backup: &Path,
	cause: Error,
) -> Error {
	if let Err(recovery_error) = exchange(&output.path, path) {
		output.armed = false;
		backup_guard.armed = false;

		return err!(
			"Output `{}` changed after replacement validation failed: {cause} Restoring it also \
			 failed: {recovery_error} Recover from `{}`; the displaced target remains at `{}`.",
			path.display(),
			backup.display(),
			output.path.display(),
		);
	}

	let parent = backup
		.parent()
		.filter(|parent| !parent.as_os_str().is_empty())
		.unwrap_or_else(|| Path::new("."));

	discard_backup(backup, parent, cause)
}

fn discard_backup(backup: &Path, parent: &Path, cause: Error) -> Error {
	match remove_file(backup) {
		| Ok(()) => {},
		| Err(error) if error.kind() == ErrorKind::NotFound => {},
		| Err(error) => {
			return err!(
				"{cause} Output was not changed, but the staged backup remains at `{}` because \
				 it could not be removed: {error}",
				backup.display(),
			);
		},
	}

	if let Err(error) = sync_parent(parent) {
		return err!(
			"{cause} Output was not changed, but backup cleanup could not be synchronized: \
			 {error}",
		);
	}

	cause
}

fn install_noreplace(source: &Path, target: &Path, kind: &str) -> Result {
	match hard_link(source, target) {
		| Ok(()) => Ok(()),
		| Err(error) if error.kind() != ErrorKind::AlreadyExists =>
			Err(fs_pair_error(&error, format_args!("install {kind}"), source, target)),
		| Err(_) => {
			Err!("{} already exists: {}.", title(kind), target.display())
		},
	}
}

fn title(kind: &str) -> &str {
	match kind {
		| "output" => "Output",
		| "backup" => "Backup",
		| _ => "Destination",
	}
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn exchange(left: &Path, right: &Path) -> Result {
	let left_name = path_cstring(left)?;
	let right_name = path_cstring(right)?;

	swap(&left_name, &right_name).map_err(|error| fs_pair_error(&error, "exchange", left, right))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn exchange(left: &Path, right: &Path) -> Result {
	Err!(
		"Safe forced replacement of `{}` with `{}` is unsupported on this platform.",
		right.display(),
		left.display(),
	)
}

// The raw syscall stands in for the renameat2 wrapper musl never provides,
// though the libc crate declares it for every Linux target.
#[cfg(target_os = "linux")]
fn swap(left: &CStr, right: &CStr) -> IoResult<()> {
	// SAFETY: Both paths stay borrowed for the call and are NUL-terminated. The
	// kernel copies them and reports every path condition through errno.
	let result = unsafe {
		syscall(
			SYS_renameat2,
			AT_FDCWD,
			left.as_ptr(),
			AT_FDCWD,
			right.as_ptr(),
			RENAME_EXCHANGE,
		)
	};

	(result == 0)
		.then_some(())
		.ok_or_else(IoError::last_os_error)
}

#[cfg(target_os = "macos")]
fn swap(left: &CStr, right: &CStr) -> IoResult<()> {
	// SAFETY: Both paths stay borrowed for the call and are NUL-terminated. The
	// kernel copies them and reports every path condition through errno.
	let result = unsafe { renamex_np(left.as_ptr(), right.as_ptr(), RENAME_SWAP) };

	(result == 0)
		.then_some(())
		.ok_or_else(IoError::last_os_error)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn path_cstring(path: &Path) -> Result<CString> {
	CString::new(path.as_os_str().as_bytes())
		.map_err(|error| err!("Invalid output path `{}`: {error}", path.display()))
}

#[cfg(unix)]
fn same_file(left: &Metadata, right: &Metadata) -> bool {
	left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(_left: &Metadata, _right: &Metadata) -> bool { false }

#[expect(
	clippy::filetype_is_file,
	reason = "Every nonregular output target must be rejected, including devices and sockets."
)]
fn is_nonregular(metadata: &Metadata) -> bool { !metadata.file_type().is_file() }

fn create_temp(parent: &Path, target: &Path) -> Result<(PathBuf, File)> {
	let filename = target
		.file_name()
		.ok_or_else(|| err!("Output path has no file name: {}.", target.display()))?;

	loop {
		let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
		let mut name = OsString::from(".");

		name.push(filename);
		name.push(format!(".{}.{}.tmp", id(), sequence));
		let path = parent.join(name);
		let mut options = OpenOptions::new();

		options.read(true).write(true).create_new(true);

		#[cfg(unix)]
		options.mode(0o600);

		match options.open(&path) {
			| Ok(file) => return Ok((path, file)),
			| Err(error) if error.kind() == ErrorKind::AlreadyExists => {},
			| Err(error) => return Err(fs_error(&error, "create temporary output", &path)),
		}
	}
}

fn backup_path(path: &Path) -> PathBuf {
	let mut backup = path.as_os_str().to_owned();

	backup.push(".bak");

	backup.into()
}

#[cfg(unix)]
fn set_output_metadata(file: &File, metadata: Option<&Metadata>, path: &Path) -> Result {
	let Some(metadata) = metadata else {
		file.set_permissions(Permissions::from_mode(0o600))
			.map_err(|error| fs_error(&error, "set temporary output permissions", path))?;

		return Ok(());
	};

	set_output_owner(file, metadata, path)?;

	file.set_permissions(Permissions::from_mode(metadata.mode()))
		.map_err(|error| {
			fs_error(&error, "preserve output permissions on temporary file", path)
		})?;

	Ok(())
}

#[cfg(not(unix))]
fn set_output_metadata(file: &File, metadata: Option<&Metadata>, path: &Path) -> Result {
	if let Some(metadata) = metadata {
		file.set_permissions(metadata.permissions())
			.map_err(|error| {
				fs_error(&error, "preserve output permissions on temporary file", path)
			})?;
	}

	Ok(())
}

#[cfg(unix)]
fn set_output_owner(file: &File, origin: &Metadata, path: &Path) -> Result {
	let staged = file
		.metadata()
		.map_err(|error| fs_error(&error, "inspect temporary output", path))?;

	let (uid, gid) = (origin.uid(), origin.gid());

	// A sandbox can deny the chown syscall even where it would change nothing.
	if staged.uid() == uid && staged.gid() == gid {
		return Ok(());
	}

	// SAFETY: The descriptor is live and the numeric IDs were supplied by the OS.
	let result = unsafe { fchown(file.as_raw_fd(), uid, gid) };

	(result == 0)
		.then_some(())
		.ok_or_else(IoError::last_os_error)
		.map_err(|error| fs_error(&error, "preserve output ownership on temporary file", path))
}

// Applied after commit so a failed replacement never installs a backup whose
// copied entries could refuse deletion during cleanup.
#[cfg(target_os = "macos")]
fn preserve_backup_attributes(source: &File, backup: &File, path: &Path) {
	copy_security_attributes(source, backup, path)
		.and_then(|()| {
			backup
				.sync_all()
				.map_err(|error| fs_error(&error, "synchronize backup attributes", path))
		})
		.inspect_err(|error| {
			warn!(
				?error,
				path = %path.display(),
				"Backup was installed, but its security attributes could not be preserved.",
			);
		})
		.ok();
}

#[cfg(not(target_os = "macos"))]
fn preserve_backup_attributes(_source: &File, _backup: &File, _path: &Path) {}

#[cfg(target_os = "macos")]
fn copy_security_attributes(source: &File, output: &File, path: &Path) -> Result {
	// SAFETY: Both descriptors are live for the call and the null state selects
	// the default copy behavior.
	let result = unsafe {
		fcopyfile(
			source.as_raw_fd(),
			output.as_raw_fd(),
			null_mut(),
			COPYFILE_ACL | COPYFILE_XATTR,
		)
	};

	(result == 0)
		.then_some(())
		.ok_or_else(IoError::last_os_error)
		.map_err(|error| fs_error(&error, "preserve security attributes", path))
}

#[cfg(not(target_os = "macos"))]
fn copy_security_attributes(_source: &File, _output: &File, _path: &Path) -> Result { Ok(()) }

#[cfg(unix)]
fn sync_parent(parent: &Path) -> Result {
	let directory =
		File::open(parent).map_err(|error| fs_error(&error, "open output directory", parent))?;

	directory
		.sync_all()
		.map_err(|error| fs_error(&error, "synchronize output directory", parent))?;

	Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> Result { Ok(()) }

fn sync_after_commit(parent: &Path, path: &Path) {
	sync_parent(parent)
		.inspect_err(|error| {
			debug_warn!(
				?error,
				path = %path.display(),
				"Config output was installed, but its directory could not be synchronized.",
			);
		})
		.ok();
}

fn cleanup_after_commit(guard: &mut TempGuard) {
	guard
		.remove()
		.inspect_err(|error| {
			debug_warn!(
				?error,
				path = %guard.path.display(),
				"Config output was installed, but a temporary file could not be removed.",
			);
		})
		.ok();
}

fn entry_exists(path: &Path) -> Result<bool> {
	match symlink_metadata(path) {
		| Ok(_) => Ok(true),
		| Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
		| Err(error) => Err(fs_error(&error, "inspect backup path", path)),
	}
}

fn fs_error(error: &IoError, operation: &str, path: &Path) -> Error {
	err!("Failed to {operation} `{}`: {error}", path.display())
}

fn fs_pair_error(
	error: &IoError,
	operation: impl Display,
	source: &Path,
	target: &Path,
) -> Error {
	err!(
		"Failed to {operation} `{}` as `{}`: {error}",
		source.display(),
		target.display(),
	)
}

impl TempGuard {
	fn remove(&mut self) -> IoResult<()> {
		match remove_file(&self.path) {
			| Ok(()) => self.armed = false,
			| Err(error) if error.kind() == ErrorKind::NotFound => self.armed = false,
			| Err(error) => return Err(error),
		}

		Ok(())
	}
}

impl Drop for TempGuard {
	fn drop(&mut self) {
		if !self.armed {
			return;
		}

		match remove_file(&self.path) {
			| Ok(()) => {},
			| Err(error) if error.kind() == ErrorKind::NotFound => {},
			| Err(error) => {
				debug_warn!(
					?error,
					path = %self.path.display(),
					"Failed to remove temporary config output.",
				);
			},
		}
	}
}
