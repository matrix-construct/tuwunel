#![cfg(test)]

use std::{
	cell::Cell,
	env::temp_dir,
	fs::remove_dir_all,
	iter::repeat_with,
	path::{Path, PathBuf},
	process::id as process_id,
	sync::{
		Arc, Condvar, Mutex,
		atomic::{AtomicUsize, Ordering::SeqCst},
	},
	time::Duration,
};

use futures::{pin_mut, poll};
use image::{Frame, Rgba, RgbaImage, codecs::gif::GifEncoder};
use tokio::{
	runtime::{Builder, Handle},
	select,
	sync::{Notify, Semaphore, oneshot::channel as oneshot_channel},
	task::JoinHandle,
	task_local,
	time::timeout,
};
use tracing::subscriber::NoSubscriber;
use tuwunel_core::{
	Result, Server,
	config::{Config, Figment, Sources},
	implement,
	log::{LogLevelReloadHandles, Logging, capture::State},
	metrics::Metrics,
	ruma::Mxc,
};

use super::{super::data::Metadata, Animate, Dim};
use crate::Services;

const SOURCE: &[u8] = b"not a picture";

#[derive(Clone, Copy, Eq, PartialEq)]
enum Pause {
	BeforeSubmit,
	Worker,
	Caller,
}

#[derive(Default)]
struct Event {
	count: AtomicUsize,
	notify: Notify,
}

#[derive(Default)]
pub(super) struct BlockingGate {
	open: Mutex<bool>,
	notify: Condvar,
}

struct AsyncGate(Semaphore);

pub(super) struct AnimationControl {
	pause: Pause,
	before_submit: Event,
	submitted: Event,
	started: Event,
	worker_finished: Event,
	caller: Event,
	submit_gate: AsyncGate,
	worker_gate: BlockingGate,
	caller_gate: AsyncGate,
}

struct ReleaseWorkerOnDrop(Arc<AnimationControl>);

struct OpenOnDrop(Arc<BlockingGate>);

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

task_local! {
	static SOURCE_FETCHES: Cell<usize>;
	static ANIMATION_CONTROL: Arc<AnimationControl>;
}

pub(super) fn source_fetched() {
	SOURCE_FETCHES
		.try_with(|fetches| fetches.set(fetches.get().saturating_add(1)))
		.ok();
}

pub(super) async fn before_animation_submit() {
	if let Some(control) = animation_control() {
		control.before_submit.signal();
		if control.pause == Pause::BeforeSubmit {
			control.submit_gate.wait().await;
		}
	}
}

pub(super) fn animation_submitted() {
	if let Some(control) = animation_control() {
		control.submitted.signal();
	}
}

pub(super) async fn caller_after_animation() {
	if let Some(control) = animation_control() {
		control.caller.signal();
		if control.pause == Pause::Caller {
			control.caller_gate.wait().await;
		}
	}
}

pub(super) fn animation_control() -> Option<Arc<AnimationControl>> {
	ANIMATION_CONTROL.try_with(Arc::clone).ok()
}

#[implement(Event)]
fn signal(&self) {
	self.count.fetch_add(1, SeqCst);
	self.notify.notify_one();
}

#[implement(Event)]
async fn wait(&self) {
	while self.count.load(SeqCst) == 0 {
		self.notify.notified().await;
	}
}

#[implement(Event)]
fn count(&self) -> usize { self.count.load(SeqCst) }

#[implement(BlockingGate)]
pub(super) fn wait(&self) {
	let open = self.open.lock().expect("gate lock is available");
	let _open = self
		.notify
		.wait_while(open, |open| !*open)
		.expect("gate lock remains available");
}

#[implement(BlockingGate)]
fn open(&self) {
	*self.open.lock().expect("gate lock is available") = true;
	self.notify.notify_all();
}

#[implement(AsyncGate)]
fn new() -> Self { Self(Semaphore::new(0)) }

#[implement(AsyncGate)]
async fn wait(&self) { let _permit = self.0.acquire().await; }

#[implement(AsyncGate)]
fn open(&self) { self.0.close(); }

#[implement(AnimationControl)]
fn new(pause: Pause) -> Arc<Self> {
	Arc::new(Self {
		pause,
		before_submit: Event::default(),
		submitted: Event::default(),
		started: Event::default(),
		worker_finished: Event::default(),
		caller: Event::default(),
		submit_gate: AsyncGate::new(),
		worker_gate: BlockingGate::default(),
		caller_gate: AsyncGate::new(),
	})
}

#[implement(AnimationControl)]
pub(super) fn worker_started(&self) {
	self.started.signal();
	if self.pause == Pause::Worker {
		self.worker_gate.wait();
	}
}

#[implement(AnimationControl)]
pub(super) fn worker_finished(&self) { self.worker_finished.signal(); }

impl Drop for ReleaseWorkerOnDrop {
	fn drop(&mut self) { self.0.worker_gate.open(); }
}

impl Drop for OpenOnDrop {
	fn drop(&mut self) { self.0.open(); }
}

#[tokio::test]
async fn duplicate_waiters_do_not_fetch_before_admission() -> Result {
	let db_path = database_path("duplicate-waiters");
	let services = build_services(&db_path.0).await?;
	let mxc = Mxc {
		server_name: services.globals.server_name(),
		media_id: "source-admission",
	};

	assert!(
		!services
			.media
			.create(&mxc, None, None, Some("text/plain"), SOURCE)
			.await?
	);

	let first_data = services.media.original_metadata(&mxc).await?;
	let cancelled_data = services.media.original_metadata(&mxc).await?;
	let dim = Dim::new(32, 32, None);
	let slots = Arc::clone(&services.media.animated_thumbnail_slots);
	let held = slots
		.clone()
		.acquire_owned()
		.await
		.expect("admission semaphore is open");

	SOURCE_FETCHES
		.scope(Cell::new(0), async {
			let first =
				services
					.media
					.get_thumbnail_generate(&mxc, &dim, Animate::Never, first_data);

			pin_mut!(first);

			assert!(poll!(first.as_mut()).is_pending());

			{
				let cancelled = services.media.get_thumbnail_generate(
					&mxc,
					&dim,
					Animate::Never,
					cancelled_data,
				);

				pin_mut!(cancelled);

				assert!(poll!(cancelled.as_mut()).is_pending());
				assert_eq!(SOURCE_FETCHES.with(Cell::get), 0);
			};

			drop(held);

			let media = first.await?;

			assert_eq!(media.content, SOURCE);
			assert_eq!(SOURCE_FETCHES.with(Cell::get), 1);

			let permit = slots
				.clone()
				.try_acquire_owned()
				.expect("cancelled waiter did not leak admission");

			drop(permit);

			Ok(())
		})
		.await
}

#[tokio::test]
async fn cancelled_active_caller_holds_admission_until_worker_exits() -> Result {
	let db_path = database_path("active-worker");
	let services = build_services(&db_path.0).await?;
	let source = animation(2);
	let first_data = upload(&services, "active-first", &source).await?;
	let second_data = upload(&services, "active-second", &source).await?;
	let control = AnimationControl::new(Pause::Worker);
	let dim = Dim::new(2, 2, None);
	let _release = ReleaseWorkerOnDrop(Arc::clone(&control));
	let test = async {
		let first_mxc = Mxc {
			server_name: services.globals.server_name(),
			media_id: "active-first",
		};

		{
			let first = services.media.get_thumbnail_generate(
				&first_mxc,
				&dim,
				Animate::Never,
				first_data,
			);

			pin_mut!(first);

			select! {
				result = first.as_mut() =>
					panic!("request completed before worker barrier: {}", result.is_ok()),
				() = control.started.wait() => {},
			}
		};

		assert_eq!(SOURCE_FETCHES.with(Cell::get), 1);

		let second_mxc = Mxc {
			server_name: services.globals.server_name(),
			media_id: "active-second",
		};

		let second =
			services
				.media
				.get_thumbnail_generate(&second_mxc, &dim, Animate::Never, second_data);

		pin_mut!(second);

		assert!(poll!(second.as_mut()).is_pending());
		assert_eq!(SOURCE_FETCHES.with(Cell::get), 1);

		control.worker_gate.open();

		timeout(Duration::from_secs(10), second)
			.await
			.expect("next request progresses after worker exit")?;

		assert_eq!(SOURCE_FETCHES.with(Cell::get), 2);

		let slots = Arc::clone(&services.media.animated_thumbnail_slots);
		let permit = slots
			.try_acquire_owned()
			.expect("active cancellation does not leak admission");

		drop(permit);

		Ok(())
	};

	run_controlled(Arc::clone(&control), test).await
}

#[tokio::test]
async fn completed_worker_keeps_admission_while_caller_owns_source() -> Result {
	let db_path = database_path("caller-source");
	let services = build_services(&db_path.0).await?;
	let source = animation(2);
	let first_data = upload(&services, "caller-first", &source).await?;
	let second_data = upload(&services, "caller-second", &source).await?;
	let control = AnimationControl::new(Pause::Caller);
	let dim = Dim::new(2, 2, None);
	let test = async {
		let first_mxc = Mxc {
			server_name: services.globals.server_name(),
			media_id: "caller-first",
		};

		let first =
			services
				.media
				.get_thumbnail_generate(&first_mxc, &dim, Animate::Never, first_data);

		pin_mut!(first);

		select! {
			result = first.as_mut() =>
				panic!("request completed before caller barrier: {}", result.is_ok()),
			() = control.caller.wait() => {},
		}

		assert_eq!(control.worker_finished.count(), 1);
		assert_eq!(SOURCE_FETCHES.with(Cell::get), 1);

		let second_mxc = Mxc {
			server_name: services.globals.server_name(),
			media_id: "caller-second",
		};

		let second =
			services
				.media
				.get_thumbnail_generate(&second_mxc, &dim, Animate::Never, second_data);

		pin_mut!(second);

		assert!(poll!(second.as_mut()).is_pending());
		assert_eq!(SOURCE_FETCHES.with(Cell::get), 1);

		control.caller_gate.open();

		timeout(Duration::from_secs(10), first)
			.await
			.expect("first request leaves caller barrier")?;

		timeout(Duration::from_secs(10), second)
			.await
			.expect("next request progresses after caller returns")?;

		assert_eq!(SOURCE_FETCHES.with(Cell::get), 2);

		let slots = Arc::clone(&services.media.animated_thumbnail_slots);
		let permit = slots
			.try_acquire_owned()
			.expect("caller retention does not leak admission");

		drop(permit);

		Ok(())
	};

	run_controlled(Arc::clone(&control), test).await
}

#[test]
fn cancelled_queued_worker_releases_admission_without_starting() {
	let runtime = Builder::new_current_thread()
		.enable_all()
		.max_blocking_threads(1)
		.build()
		.expect("test runtime builds");

	runtime
		.block_on(queued_worker_cancellation())
		.expect("queued cancellation completes");
}

async fn queued_worker_cancellation() -> Result {
	let db_path = database_path("queued-worker");
	let services = build_services(&db_path.0).await?;
	let source = animation(2);
	let first_data = upload(&services, "queued-first", &source).await?;
	let second_data = upload(&services, "queued-second", &source).await?;
	let control = AnimationControl::new(Pause::BeforeSubmit);
	let dim = Dim::new(2, 2, None);
	let pool_gate = Arc::new(BlockingGate::default());
	let _release = OpenOnDrop(Arc::clone(&pool_gate));
	let test = async {
		let blocker =
			cancel_queued_request(&services, &control, &pool_gate, &dim, first_data).await;

		pool_gate.open();
		blocker.await?;

		assert_eq!(control.started.count(), 0);

		let slots = Arc::clone(&services.media.animated_thumbnail_slots);
		let permit = timeout(Duration::from_secs(10), slots.acquire_owned())
			.await
			.expect("queued cancellation returns admission")
			.expect("admission semaphore is open");

		drop(permit);

		let second_mxc = Mxc {
			server_name: services.globals.server_name(),
			media_id: "queued-second",
		};

		let second =
			services
				.media
				.get_thumbnail_generate(&second_mxc, &dim, Animate::Never, second_data);

		timeout(Duration::from_secs(10), second)
			.await
			.expect("request progresses after queued cancellation")?;

		assert_eq!(SOURCE_FETCHES.with(Cell::get), 2);
		assert_eq!(control.started.count(), 1);
		assert_eq!(control.worker_finished.count(), 1);

		Ok(())
	};

	run_controlled(Arc::clone(&control), test).await
}

async fn cancel_queued_request(
	services: &Services,
	control: &AnimationControl,
	pool_gate: &Arc<BlockingGate>,
	dim: &Dim,
	data: Metadata,
) -> JoinHandle<()> {
	let blocker;

	{
		let mxc = Mxc {
			server_name: services.globals.server_name(),
			media_id: "queued-first",
		};

		let request = services
			.media
			.get_thumbnail_generate(&mxc, dim, Animate::Never, data);

		pin_mut!(request);

		select! {
			result = request.as_mut() =>
				panic!("request completed before submission barrier: {}", result.is_ok()),
			() = control.before_submit.wait() => {},
		}

		assert_eq!(SOURCE_FETCHES.with(Cell::get), 1);

		let (started, start) = oneshot_channel();
		let gate = Arc::clone(pool_gate);

		blocker = Handle::current().spawn_blocking(move || {
			let _sent = started.send(());
			gate.wait();
		});

		start.await.expect("pool blocker starts");
		control.submit_gate.open();

		select! {
			result = request.as_mut() =>
				panic!("queued animation completed before cancellation: {}", result.is_ok()),
			() = control.submitted.wait() => {},
		}
	};

	blocker
}

fn database_path(name: &str) -> DatabasePath {
	DatabasePath(
		temp_dir()
			.join("tuwunel")
			.join(format!("tuwunel-media-admission-{}-{name}", process_id())),
	)
}

async fn build_services(path: &Path) -> Result<Arc<Services>> {
	let path = path.to_string_lossy();
	let raw_config = Figment::new()
		.merge(("server_name", "localhost"))
		.merge(("database_path", path.as_ref()))
		.merge(("media_thumbnail_animated", true))
		.merge(("media_thumbnail_animated_concurrency", 1))
		.merge(("test", ["fresh", "cleanup"]));

	let config = Config::new(&raw_config)?;
	let runtime = Handle::current();
	let logging = Logging {
		subscriber: Arc::new(NoSubscriber::new()),
		reload: LogLevelReloadHandles::default(),
		capture: Arc::new(State::new()),
	};

	let metrics = Metrics::new(Some(&runtime));
	let server =
		Arc::new(Server::new(config, Sources::default(), Some(&runtime), logging, metrics));

	Services::build(server).await
}

fn animation(frames: usize) -> Vec<u8> {
	let buffer = RgbaImage::from_pixel(4, 4, Rgba([255, 0, 0, 255]));
	let mut content = Vec::new();
	let mut encoder = GifEncoder::new(&mut content);

	encoder
		.encode_frames(repeat_with(|| Frame::new(buffer.clone())).take(frames))
		.expect("test animation encodes");

	drop(encoder);

	content
}

async fn upload(services: &Services, media_id: &str, source: &[u8]) -> Result<Metadata> {
	let mxc = Mxc {
		server_name: services.globals.server_name(),
		media_id,
	};

	assert!(
		services
			.media
			.create(&mxc, None, None, Some("text/plain"), source)
			.await?
	);

	services.media.original_metadata(&mxc).await
}

async fn run_controlled<F>(control: Arc<AnimationControl>, future: F) -> F::Output
where
	F: Future,
{
	SOURCE_FETCHES
		.scope(Cell::new(0), ANIMATION_CONTROL.scope(control, future))
		.await
}
