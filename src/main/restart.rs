#![cfg(unix)]

use std::{
	env::{args, vars},
	mem::take,
	os::unix::process::CommandExt,
	process::Command,
};

use tuwunel_core::{debug, info, utils};

const RESTORE_BACKUP: &str = "--restore-backup";

#[cold]
pub fn restart() -> ! {
	let exe = utils::sys::current_exe().expect("program path must be available");
	let envs = vars();
	let args: Vec<_> = strip_restore_backup(args().skip(1)).collect();
	debug!(?exe, ?args, ?envs, "Restart");

	info!("Restart");

	let error = Command::new(exe).args(args).envs(envs).exec();
	panic!("{error:?}");
}

/// Restoring a backup is one-shot for the invocation which asked for it;
/// carrying `--restore-backup` into the next image would restore again over
/// everything written since.
fn strip_restore_backup(args: impl Iterator<Item = String>) -> impl Iterator<Item = String> {
	args.scan(false, |bare, arg| {
		// A bare flag takes the next argument, unless that is itself a flag.
		let value = take(bare) && !arg.starts_with('-');

		*bare = arg == RESTORE_BACKUP;
		let flag = arg.split('=').next() == Some(RESTORE_BACKUP);

		Some((!value && !flag).then_some(arg))
	})
	.flatten()
}

#[cfg(test)]
mod tests {
	use super::strip_restore_backup;

	fn stripped(args: &[&str]) -> Vec<String> {
		strip_restore_backup(args.iter().copied().map(str::to_owned)).collect()
	}

	#[test]
	fn restore_backup_never_survives() {
		assert!(stripped(&["--restore-backup"]).is_empty());
		assert!(stripped(&["--restore-backup", "5"]).is_empty());
		assert!(stripped(&["--restore-backup=5"]).is_empty());
	}

	#[test]
	fn other_arguments_survive() {
		assert_eq!(stripped(&["--restore-backup", "--read-only"]), ["--read-only"]);
		assert_eq!(stripped(&["--restore-backup", "5", "--read-only"]), ["--read-only"]);
		assert_eq!(stripped(&["--read-only", "--restore-backup"]), ["--read-only"]);
		assert_eq!(stripped(&["--execute", "5"]), ["--execute", "5"]);
	}
}
