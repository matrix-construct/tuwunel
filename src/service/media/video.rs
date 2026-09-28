//! Video Thumbnails
//!
//! Tuwunel decodes no video itself. An operator-configured program extracts a
//! still frame, which the image thumbnailer then treats as the source picture.

use std::{
	borrow::Cow,
	fs::{read_dir, remove_file},
	io,
	path::Path,
	process::Stdio,
	time::Duration,
};

use futures::future::try_join;
#[cfg(unix)]
use libc::{SIGKILL, killpg};
use lru_cache::LruCache;
use ruma::Mxc;
use tokio::{
	fs::{OpenOptions, create_dir_all},
	io::{AsyncRead, AsyncReadExt, AsyncWriteExt, copy, sink},
	process::Command,
	time::{Instant, timeout_at},
};
use tuwunel_core::{
	Config, Err, Result, debug, debug_warn, defer, err, implement,
	utils::{BoolExt, math::u64_from_usize_saturating, random_string},
};

use super::{Dim, Media};

/// Tokens replaced in every configured argument before each call.
const INPUT: &str = "{input}";
const WIDTH: &str = "{width}";
const HEIGHT: &str = "{height}";

/// Content-type prefix of media a still frame is extracted from.
const VIDEO: &str = "video/";

/// Distinguishes a staged video from anything else in the staging directory,
/// so the startup sweep reclaims only what this module wrote.
const STAGED: &str = "tuwunel-video-";

const NAME_LENGTH: usize = 16;

const DIAGNOSTIC_LEN: u64 = 4096;

/// How long a video whose extraction failed is left alone. Retrying it at once
/// would spend a slot to fail again, and at the default concurrency of one that
/// is the slot every other video is waiting for.
const FAILURE_COOLDOWN: Duration = Duration::from_mins(5);

/// Videos remembered as having failed.
pub(super) const FAILURES: usize = 1024;

/// Fraction of the deadline the program must still have to be worth running,
/// and to be answerable for missing.
const FAIR_SHARE: f64 = 4.0;

/// Videos whose frame extraction failed, against the time it last did.
/// Bounded, since forgetting an entry early only costs a retry.
pub(super) type Failures = LruCache<String, Instant>;

/// Kills the program's process group on every exit that leaves it running, so
/// that a shutdown or a disconnected client takes a wrapper's descendants with
/// it exactly as an expired deadline does. Disarmed once the child is reaped,
/// past which the identifier could name a group this server never spawned.
struct Reaper {
	group: Option<u32>,
}

impl Reaper {
	fn disarm(&mut self) { self.group = None; }
}

impl Drop for Reaper {
	fn drop(&mut self) {
		if let Some(group) = self.group {
			kill_group(group);
		}
	}
}

/// Still frame standing in for a video, or `None` when the media is not a
/// video, no program is configured, or extraction failed.
#[implement(super::Service)]
#[tracing::instrument(
	name = "video",
	level = "debug",
	skip_all,
	fields(
		?dim,
		content_type = ?media.content_type,
	),
)]
pub(super) async fn video_frame(
	&self,
	mxc: &Mxc<'_>,
	dim: &Dim,
	media: &Media,
) -> Option<Vec<u8>> {
	let config = &self.services.config;

	let is_video = media
		.content_type
		.as_deref()
		.is_some_and(|content_type| content_type.starts_with(VIDEO));

	let workable = !config.media_video_thumbnail_command.is_empty()
		&& media.content.len() <= config.media_video_thumbnail_max_size
		&& !self.failed_recently(mxc);

	is_video
		.and_is(workable)
		.then_async(|| self.extract_frame(mxc, dim, &media.content))
		.await?
		.inspect_err(|e| debug_warn!(%e, "Failed to extract a video frame."))
		.ok()
}

/// Whether this video failed within the cooldown, where trying again would
/// spend a slot to reach the same failure.
#[implement(super::Service)]
fn failed_recently(&self, mxc: &Mxc<'_>) -> bool {
	let Some(cooldown) = Instant::now().checked_sub(FAILURE_COOLDOWN) else {
		return false;
	};

	self.video_thumbnail_failures
		.lock()
		.ok()
		.and_then(|mut failures| failures.get_mut(&mxc.to_string()).copied())
		.is_some_and(|failed| failed > cooldown)
}

#[implement(super::Service)]
pub(super) fn remember_failure(&self, mxc: &Mxc<'_>) {
	if let Ok(mut failures) = self.video_thumbnail_failures.lock() {
		failures.insert(mxc.to_string(), Instant::now());
	}
}

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self, content))]
async fn extract_frame(&self, mxc: &Mxc<'_>, dim: &Dim, content: &[u8]) -> Result<Vec<u8>> {
	let config = &self.services.config;
	let timeout = Duration::from_secs(config.media_video_thumbnail_timeout);

	// one deadline spans the wait for a slot, the staging write and the program,
	// so a queue cannot compound into a multiple of the configured timeout
	let deadline = Instant::now()
		.checked_add(timeout)
		.ok_or_else(|| {
			err!(Config("media_video_thumbnail_timeout", "Timeout is out of range."))
		})?;

	let Some((program, args)) = config.media_video_thumbnail_command.split_first() else {
		return Err!(Config("media_video_thumbnail_command", "No program is configured."));
	};

	let Ok(Ok(_slot)) = timeout_at(deadline, self.video_thumbnail_slots.acquire()).await else {
		return Err!("Timed out waiting for a video thumbnail slot.");
	};

	let path = staging_dir(config).join(format!("{STAGED}{}", random_string(NAME_LENGTH)));

	defer! {{ remove_file(&path).ok(); }}

	let Ok(staged) = timeout_at(deadline, stage(&path, content)).await else {
		return Err!("Timed out staging the video.");
	};

	staged?;

	// a residue of the deadline is not a fair trial: the program would fail on
	// time the queue spent, and be remembered below for someone else's load
	if deadline.saturating_duration_since(Instant::now()) < timeout.div_f64(FAIR_SHARE) {
		return Err!("Too little of the deadline remained to run the video thumbnail program.");
	}

	let width = dim.width.to_string();
	let height = dim.height.to_string();
	let args = args
		.iter()
		.map(|arg| substitute(arg, &path, &width, &height));

	let limit = u64_from_usize_saturating(config.media_video_thumbnail_max_size);
	let frame = run(program, args, limit, deadline).await;

	// the program reached a verdict on this video, so a failure is the video's
	// and worth remembering; the paths above are contention and say nothing
	if frame.is_err() {
		self.remember_failure(mxc);
	}

	frame
}

/// Videos are staged beside the database rather than in the system temporary
/// directory, which is commonly memory-backed and sized for small files.
fn staging_dir(config: &Config) -> Cow<'_, Path> {
	config
		.media_video_thumbnail_path
		.as_deref()
		.map_or_else(|| config.database_path.join("tmp").into(), Cow::Borrowed)
}

/// Reclaim videos staged by a previous run: the scope guard that unlinks them
/// survives neither a kill nor the exec of an in-place restart. Runs during
/// construction, before a request can stage a file this would race, which is
/// also before the service graph exists to read the configuration through.
#[tracing::instrument(name = "sweep", level = "debug", skip_all)]
pub(super) fn sweep_staging_dir(config: &Config) {
	let Ok(dir) = read_dir(staging_dir(config).as_ref()) else {
		return;
	};

	dir.filter_map(Result::ok)
		.map(|entry| entry.path())
		.filter(|path| {
			path.file_name()
				.and_then(|name| name.to_str())
				.is_some_and(|name| name.starts_with(STAGED))
		})
		.for_each(|path| {
			debug!(?path, "Removing a video staged by a previous run.");
			remove_file(&path).ok();
		});
}

/// Write the video to a fresh private file: the program needs a seekable
/// input, and a pipe cannot serve formats whose index trails the media.
async fn stage(path: &Path, content: &[u8]) -> Result {
	if let Some(dir) = path.parent() {
		create_dir_all(dir).await?;
	}

	let mut options = OpenOptions::new();
	options.create_new(true).write(true);

	#[cfg(unix)]
	options.mode(0o600);

	let mut file = options.open(path).await?;

	file.write_all(content).await?;
	file.flush().await?;

	Ok(())
}

pub(super) fn substitute<'a>(
	arg: &'a str,
	input: &Path,
	width: &str,
	height: &str,
) -> Cow<'a, str> {
	let substituted = || {
		arg.replace(INPUT, &input.to_string_lossy())
			.replace(WIDTH, width)
			.replace(HEIGHT, height)
			.into()
	};

	arg.contains('{')
		.then(substituted)
		.unwrap_or(Cow::Borrowed(arg))
}

/// Collect the frame the program writes to standard output, refusing one past
/// the limit rather than decoding a truncation. The program runs in its own
/// process group, so a wrapper overrunning the deadline cannot orphan the work
/// it spawned.
#[tracing::instrument(
	level = "debug",
	skip(args),
	fields(
		%program,
		%limit,
	),
)]
pub(super) async fn run<Args>(
	program: &str,
	args: Args,
	limit: u64,
	deadline: Instant,
) -> Result<Vec<u8>>
where
	Args: IntoIterator<Item: AsRef<str>> + Send,
{
	let mut command = Command::new(program);
	command
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.kill_on_drop(true);

	for arg in args {
		command.arg(arg.as_ref());
	}

	#[cfg(unix)]
	command.process_group(0);

	let mut child = command.spawn()?;
	let mut reaper = Reaper { group: child.id() };

	let stdout = child.stdout.take().expect("stdout is piped");
	let stderr = child.stderr.take().expect("stderr is piped");

	// one byte past the limit distinguishes an exact fit from a truncation
	let collect = try_join(drain(stdout, limit.saturating_add(1)), drain(stderr, DIAGNOSTIC_LEN));

	// the child is reaped only once both pipes reach EOF, so that a wrapper
	// exiting while a descendant still holds them does not read as finished
	let outcome = timeout_at(deadline, async {
		let collected = collect.await?;
		let status = child.wait().await?;

		Ok::<_, io::Error>((collected, status))
	})
	.await;

	// tokio clears the identifier on reaping, which by the above means nothing
	// still holds the pipes; signalling the group past that point could reach
	// one this server never spawned, so let a descendant that closed them go
	if child.id().is_none() {
		reaper.disarm();
	}

	let Ok(exited) = outcome else {
		return Err!("Video thumbnail program exceeded its deadline.");
	};

	let ((frame, diagnostic), status) = exited?;
	let diagnostic = String::from_utf8_lossy(&diagnostic);

	debug!(?status, len = %frame.len(), "Video thumbnail program exited.");

	if !status.success() {
		return Err!("Video thumbnail program failed with {status}: {diagnostic}");
	}

	if frame.is_empty() {
		return Err!("Video thumbnail program produced no frame: {diagnostic}");
	}

	if u64_from_usize_saturating(frame.len()) > limit {
		return Err!("Video thumbnail program produced a frame past {limit} bytes.");
	}

	Ok(frame)
}

async fn drain<Pipe>(mut pipe: Pipe, limit: u64) -> Result<Vec<u8>, io::Error>
where
	Pipe: AsyncRead + Unpin + Send,
{
	let mut buf = Vec::new();

	(&mut pipe)
		.take(limit)
		.read_to_end(&mut buf)
		.await?;

	// the pipe stays open past the limit: closing it signals a program that is
	// still writing, turning an oversized frame or a chatty log into a kill
	copy(&mut pipe, &mut sink()).await?;

	Ok(buf)
}

#[cfg(unix)]
fn kill_group(group: u32) {
	let Ok(group) = i32::try_from(group) else {
		return;
	};

	// SAFETY: the group holds only what this child spawned, and its identifier
	// cannot be reused before the child is reaped, so the signal reaches that
	// subtree alone.
	unsafe {
		killpg(group, SIGKILL);
	}
}

#[cfg(not(unix))]
fn kill_group(_group: u32) {}
