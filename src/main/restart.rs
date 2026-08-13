#![cfg(unix)]

use std::{
	env::{args, var_os, vars},
	mem::take,
	os::unix::process::CommandExt,
	process::Command,
};

use tuwunel_core::{debug, info, utils, warn};

const RESTORE_BACKUP: &str = "--restore-backup";

const LISTEN_VARS: [&str; 3] = ["LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"];

#[cold]
pub fn restart() -> ! {
	let exe = utils::sys::current_exe().expect("program path must be available");
	let envs: Vec<_> = strip_listen_fds(vars()).collect();
	let args: Vec<_> = strip_restore_backup(args().skip(1)).collect();

	debug!(?exe, ?args, ?envs, "Restart");

	if LISTEN_VARS
		.iter()
		.any(|name| var_os(name).is_some())
	{
		warn!(
			"Sockets from the service manager are not carried across this restart; the next \
			 image listens on its configured addresses alone until the service is restarted \
			 through the service manager."
		);
	}

	info!("Restart");

	// The environment is inherited whole unless cleared first, which would
	// carry back the variables stripped above.
	let error = Command::new(exe)
		.args(args)
		.env_clear()
		.envs(envs)
		.exec();

	panic!("{error:?}");
}

/// The exec keeps the process id and closes the descriptors the service
/// manager passed, so the next image would accept that handover as its own and
/// read whatever has since taken their numbers.
fn strip_listen_fds(
	envs: impl IntoIterator<Item = (String, String)>,
) -> impl Iterator<Item = (String, String)> {
	envs.into_iter()
		.filter(|(name, _)| !LISTEN_VARS.contains(&name.as_str()))
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
	use super::{strip_listen_fds, strip_restore_backup};

	fn stripped(args: &[&str]) -> Vec<String> {
		strip_restore_backup(args.iter().copied().map(str::to_owned)).collect()
	}

	fn kept(envs: &[(&str, &str)]) -> Vec<(String, String)> {
		let envs = envs
			.iter()
			.map(|&(name, value)| (name.to_owned(), value.to_owned()));

		strip_listen_fds(envs).collect()
	}

	#[test]
	fn listen_fds_never_survive() {
		let envs = kept(&[
			("LISTEN_PID", "1"),
			("LISTEN_FDS", "2"),
			("LISTEN_FDNAMES", "a:b"),
			("TUWUNEL_CONFIG", "/etc/tuwunel/tuwunel.toml"),
		]);

		assert_eq!(envs, [("TUWUNEL_CONFIG".to_owned(), "/etc/tuwunel/tuwunel.toml".to_owned())]);
	}

	#[test]
	fn restore_backup_never_survives() {
		let stripped_bare = stripped(&["--restore-backup"]);
		let stripped_valued = stripped(&["--restore-backup", "5"]);
		let stripped_inlined = stripped(&["--restore-backup=5"]);

		assert!(stripped_bare.is_empty(), "{stripped_bare:?}");
		assert!(stripped_valued.is_empty(), "{stripped_valued:?}");
		assert!(stripped_inlined.is_empty(), "{stripped_inlined:?}");
	}

	#[test]
	fn other_arguments_survive() {
		assert_eq!(stripped(&["--restore-backup", "--read-only"]), ["--read-only"]);
		assert_eq!(stripped(&["--restore-backup", "5", "--read-only"]), ["--read-only"]);
		assert_eq!(stripped(&["--read-only", "--restore-backup"]), ["--read-only"]);
		assert_eq!(stripped(&["--execute", "5"]), ["--execute", "5"]);
	}
}
