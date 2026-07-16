//! In-memory background-task tracker for the Synapse admin API.
//!
//! Long-running admin actions (room deletion, history purge, bulk redaction)
//! run detached on the runtime and are polled by their id or by the resource
//! they act on. State is process-local: a restart drops history where Synapse
//! persists it for seven days, which consumers tolerate (they poll right after
//! issuing, and a post-restart miss reads as Synapse's post-retention 404).

use std::{
	collections::BTreeMap,
	future::Future,
	sync::{Arc, Mutex as StdMutex},
	time::Duration,
};

use async_trait::async_trait;
use serde_json::Value as JsonValue;
use tokio::{task::JoinHandle, time::sleep};
use tuwunel_core::{
	Result,
	arrayvec::ArrayString,
	implement,
	utils::{rand::string_array, time::now_millis},
};

/// Random task-id length, matching Synapse's `random_string(16)`.
const TASK_ID_LEN: usize = 16;

/// Terminal tasks older than this (seven days) are pruned by the worker.
const RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// Cap on retained terminal tasks; the oldest are pruned once it is exceeded.
const CAPACITY: usize = 1024;

/// Interval between worker garbage-collection sweeps.
const GC_INTERVAL: Duration = Duration::from_hours(1);

/// A task's random id: a fixed 16-byte string kept inline.
type TaskId = ArrayString<TASK_ID_LEN>;

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	tasks: StdMutex<BTreeMap<TaskId, Task>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
	Scheduled,
	Active,
	Complete,
	Failed,
}

/// A tracked task's public snapshot, cloned out from under the lock.
#[derive(Clone, Debug)]
pub struct TaskInfo {
	pub id: TaskId,
	pub action: &'static str,
	pub resource_id: String,
	pub status: Status,
	pub timestamp_ms: u64,
	pub result: Option<JsonValue>,
	pub error: Option<String>,
}

struct Task {
	action: &'static str,
	resource_id: String,
	status: Status,
	timestamp_ms: u64,
	result: Option<JsonValue>,
	error: Option<String>,
	handle: Option<JoinHandle<()>>,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			tasks: StdMutex::new(BTreeMap::new()),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		loop {
			self.prune();

			tokio::select! {
				() = sleep(GC_INTERVAL) => {},
				() = self.services.server.until_shutdown() => return Ok(()),
			}
		}
	}

	async fn interrupt(&self) { self.abort_all(); }

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Spawn `work` on the runtime as a tracked task, returning its id. The record
/// transitions Scheduled -> Active -> Complete/Failed; `work`'s `Ok` value is
/// stored as the result, its `Err` as the error string.
#[implement(Service)]
pub fn spawn<F>(self: &Arc<Self>, action: &'static str, resource_id: String, work: F) -> TaskId
where
	F: Future<Output = Result<JsonValue>> + Send + 'static,
{
	let id = string_array::<TASK_ID_LEN>();

	// Hold the lock across the spawn+insert so the task cannot mark itself
	// Active before its record exists.
	let mut tasks = self.tasks.lock().expect("locked");
	let this = Arc::clone(self);
	let task_id = id;
	let handle = self.services.server.runtime().spawn(async move {
		this.set_active(&task_id);
		let outcome = work.await;
		this.finish(&task_id, outcome);
	});

	tasks.insert(id, Task {
		action,
		resource_id,
		status: Status::Scheduled,
		timestamp_ms: now_millis(),
		result: None,
		error: None,
		handle: Some(handle),
	});

	id
}

/// The task with this id, if it is still tracked.
#[implement(Service)]
pub fn get(&self, id: &str) -> Option<TaskInfo> {
	self.tasks
		.lock()
		.expect("locked")
		.get_key_value(id)
		.map(|(id, task)| task.info(id))
}

/// Every tracked task acting on `resource_id`, newest ordering not guaranteed.
#[implement(Service)]
pub fn by_resource(&self, resource_id: &str) -> Vec<TaskInfo> {
	self.tasks
		.lock()
		.expect("locked")
		.iter()
		.filter(|(_, task)| task.resource_id.as_str() == resource_id)
		.map(|(id, task)| task.info(id))
		.collect()
}

/// Whether a nonterminal task matches both `action` and `resource_id`.
#[implement(Service)]
pub fn has_nonterminal(&self, action: &str, resource_id: &str) -> bool {
	self.tasks
		.lock()
		.expect("locked")
		.values()
		.any(|task| matches_nonterminal(task, action, resource_id))
}

fn matches_nonterminal(task: &Task, action: &str, resource_id: &str) -> bool {
	task.action == action && task.resource_id == resource_id && !task.status.is_terminal()
}

/// Every tracked task; callers filter by action or status.
#[implement(Service)]
pub fn list(&self) -> Vec<TaskInfo> {
	self.tasks
		.lock()
		.expect("locked")
		.iter()
		.map(|(id, task)| task.info(id))
		.collect()
}

#[implement(Service)]
fn set_active(&self, id: &str) {
	if let Some(task) = self.tasks.lock().expect("locked").get_mut(id) {
		task.status = Status::Active;
	}
}

#[implement(Service)]
fn finish(&self, id: &str, outcome: Result<JsonValue>) {
	let mut tasks = self.tasks.lock().expect("locked");
	let Some(task) = tasks.get_mut(id) else {
		return;
	};

	match outcome {
		| Ok(value) => {
			task.status = Status::Complete;
			task.result = Some(value);
		},
		| Err(error) => {
			task.status = Status::Failed;
			task.error = Some(error.to_string());
		},
	}
}

#[implement(Service)]
fn prune(&self) {
	let now = now_millis();

	prune_tasks(&mut self.tasks.lock().expect("locked"), now);
}

#[implement(Service)]
fn abort_all(&self) {
	self.tasks
		.lock()
		.expect("locked")
		.values()
		.filter_map(|task| task.handle.as_ref())
		.for_each(JoinHandle::abort);
}

impl Status {
	#[must_use]
	pub fn is_terminal(self) -> bool { matches!(self, Self::Complete | Self::Failed) }

	#[must_use]
	pub fn as_str(self) -> &'static str {
		match self {
			| Self::Scheduled => "scheduled",
			| Self::Active => "active",
			| Self::Complete => "complete",
			| Self::Failed => "failed",
		}
	}
}

impl Task {
	fn info(&self, id: &TaskId) -> TaskInfo {
		TaskInfo {
			id: *id,
			action: self.action,
			resource_id: self.resource_id.clone(),
			status: self.status,
			timestamp_ms: self.timestamp_ms,
			result: self.result.clone(),
			error: self.error.clone(),
		}
	}
}

/// Drop terminal tasks past the retention window, then cap the survivors.
fn prune_tasks(tasks: &mut BTreeMap<TaskId, Task>, now_ms: u64) {
	tasks.retain(|_, task| {
		!task.status.is_terminal() || now_ms.saturating_sub(task.timestamp_ms) < RETENTION_MS
	});

	let mut timestamps: Vec<u64> = tasks
		.values()
		.filter(|task| task.status.is_terminal())
		.map(|task| task.timestamp_ms)
		.collect();

	if timestamps.len() <= CAPACITY {
		return;
	}

	timestamps.sort_unstable();

	let cutoff = timestamps[timestamps.len().saturating_sub(CAPACITY)];

	tasks.retain(|_, task| !task.status.is_terminal() || task.timestamp_ms >= cutoff);
}

#[cfg(test)]
mod tests;
