use std::{
	ops::DerefMut,
	panic::AssertUnwindSafe,
	sync::{
		Arc,
		atomic::{AtomicUsize, Ordering},
	},
	time::Duration,
};

use futures::{FutureExt, TryFutureExt};
use tokio::{
	sync::{Mutex, MutexGuard},
	task::{JoinHandle, JoinSet, yield_now},
	time::sleep,
};
use tuwunel_core::{
	Err, Error, Result, Server, debug, debug::INFO_SPAN_LEVEL, debug_warn, defer, error,
	utils::time, warn,
};

use crate::{Services, service::Service};

pub(crate) struct Manager {
	manager: Mutex<Option<JoinHandle<Result>>>,
	workers: Mutex<Workers>,
	active: AtomicUsize,
	server: Arc<Server>,
	services: Arc<Services>,
}

type Workers = JoinSet<WorkerResult>;
type WorkerResult = (Arc<dyn Service>, Result);
type WorkersLocked<'a> = MutexGuard<'a, Workers>;

const RESTART_DELAY_MS: u64 = 2500;

impl Manager {
	pub(super) fn new(services: &Arc<Services>) -> Arc<Self> {
		Arc::new(Self {
			manager: Mutex::new(None),
			workers: Mutex::new(JoinSet::new()),
			active: 0.into(),
			server: services.server.clone(),
			services: services.clone(),
		})
	}

	pub(super) async fn poll(&self) -> Result {
		if let Some(manager) = &mut *self.manager.lock().await {
			debug!("Polling service manager...");
			return manager.await?;
		}

		Ok(())
	}

	#[tracing::instrument(
		name = "manager",
		level = INFO_SPAN_LEVEL,
		skip_all,
		fields(
			active = self.active.load(Ordering::Relaxed),
		),
	)]
	pub(super) async fn stop(&self) {
		if let Some(manager) = self.manager.lock().await.take() {
			debug!("Waiting for service manager...");
			if let Err(e) = manager.await {
				error!("Manager shutdown error: {e:?}");
			}
		}
	}

	#[tracing::instrument(name = "manager", level = "trace", skip_all)]
	pub(super) async fn start(self: Arc<Self>) -> Result {
		let mut workers = self.workers.lock().await;

		debug!("Starting service manager...");
		let self_ = self.clone();
		_ = self.manager.lock().await.deref_mut().insert(
			self.server
				.runtime()
				.spawn(async move { self_.worker().await }),
		);

		yield_now().await;
		debug!("Starting service workers...");
		for service in self.services.services() {
			self.start_worker(&mut workers, &service)?;
		}

		yield_now().await;
		debug!(
			workers = workers.len(),
			active = self.active.load(Ordering::Relaxed),
			"Spawned service workers...",
		);

		debug_assert!(self.manager.lock().await.is_some(), "Manager task must be installed",);

		Ok(())
	}

	#[tracing::instrument(
		name = "manager",
		level = INFO_SPAN_LEVEL,
		skip_all,
	)]
	async fn worker(self: &Arc<Self>) -> Result {
		loop {
			let mut workers = self.workers.lock().await;
			tokio::select! {
				result = workers.join_next() => match result {
					Some(Ok(result)) => self.handle_result(&mut workers, result).await?,
					Some(Err(error)) => self.handle_abort(&mut workers, &Error::from(error))?,
					None => break,
				}
			}
		}

		debug!("Worker manager finished");
		Ok(())
	}

	#[allow(clippy::unused_self)]
	fn handle_abort(&self, _workers: &mut WorkersLocked<'_>, error: &Error) -> Result {
		// not supported until service can be associated with abort
		unimplemented!("unexpected worker task abort {error:?}");
	}

	async fn handle_result(
		self: &Arc<Self>,
		workers: &mut WorkersLocked<'_>,
		result: WorkerResult,
	) -> Result {
		let (service, result) = result;
		match result {
			| Ok(()) => self.handle_finished(workers, &service),
			| Err(error) => self.handle_error(workers, &service, error).await,
		}
	}

	#[tracing::instrument(
		name = "finished",
		level = "trace",
		skip_all,
		fields(
			service = ?service.name(),
			active = self.active.load(Ordering::Relaxed),
		),
	)]
	#[expect(clippy::unused_self)]
	fn handle_finished(
		self: &Arc<Self>,
		_workers: &mut WorkersLocked<'_>,
		service: &Arc<dyn Service>,
	) -> Result {
		debug!(name = service.name(), "Service worker finished...");

		Ok(())
	}

	#[tracing::instrument(
		name = "error",
		level = "error",
		skip_all,
		fields(
			service = ?service.name(),
			active = self.active.load(Ordering::Relaxed),
		),
	)]
	async fn handle_error(
		self: &Arc<Self>,
		workers: &mut WorkersLocked<'_>,
		service: &Arc<dyn Service>,
		error: Error,
	) -> Result {
		let name = service.name();
		error!("service {name:?} aborted: {error}");

		if !self.server.is_running() {
			debug_warn!("service {name:?} error ignored on shutdown.");
			return Ok(());
		}

		if !error.is_panic() {
			return Err(error);
		}

		let delay = Duration::from_millis(RESTART_DELAY_MS);
		warn!(
			delay = ?time::pretty(delay),
			"service {name:?} worker restarting after delay..."
		);

		sleep(delay).await;
		self.start_worker(workers, service)
	}

	/// Start the worker in a task for the service.
	fn start_worker(
		self: &Arc<Self>,
		workers: &mut WorkersLocked<'_>,
		service: &Arc<dyn Service>,
	) -> Result {
		if !self.server.is_running() {
			return Err!(
				"Service {:?} worker not starting during server shutdown.",
				service.name()
			);
		}

		debug!(name = service.name(), "Service worker starting...");
		workers.spawn_on(worker(service.clone(), self.clone()), self.server.runtime());

		Ok(())
	}
}

/// Base frame for service worker. This runs in a tokio::task. All errors and
/// panics from the worker are caught and returned cleanly. The JoinHandle
/// should never error with a panic, and if so it should propagate, but it may
/// error with an Abort which the manager should handle along with results to
/// determine if the worker should be restarted.
#[tracing::instrument(
	parent = None,
	level = "trace",
	skip_all,
	fields(
		service = ?service.name(),
		active = mgr.active.load(Ordering::Relaxed),
	),
)]
async fn worker(service: Arc<dyn Service>, mgr: Arc<Manager>) -> WorkerResult {
	mgr.active.fetch_add(1, Ordering::Relaxed);
	defer! {{
		mgr.active.fetch_sub(1, Ordering::Release);
	}};

	let service_ = Arc::clone(&service);
	let result = AssertUnwindSafe(service_.worker())
		.catch_unwind()
		.map_err(Error::from_panic);

	let result = if service.unconstrained() {
		tokio::task::unconstrained(result).await
	} else {
		result.await
	};

	// flattens JoinError for panic into worker's Error
	(service, result.unwrap_or_else(Err))
}
