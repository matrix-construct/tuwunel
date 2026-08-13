//! Exit-time dumps of runtime metrics and resource usage.
//!
//! Each file is a small JSON envelope: a `meta` block (pid, timestamp,
//! version, scope) and a `payload` string holding the Debug output of the
//! source struct verbatim.

use std::{fs, path::Path, process};

use serde::Serialize;
#[cfg(tokio_unstable)]
use tokio_metrics::RuntimeMetrics;

use crate::{
	Result, debug_info, error,
	utils::{sys::Usage, time::now_millis},
	version,
};

#[cfg(tokio_unstable)]
const RUNTIME_METRICS_PREFIX: &str = "tuwunel.runtime_metrics";
const RUNTIME_USAGE_PREFIX: &str = "tuwunel.runtime_usage";

#[derive(Serialize)]
struct Dump<'a> {
	meta: DumpMeta,
	payload: &'a str,
}

#[derive(Serialize)]
struct DumpMeta {
	pid: u32,
	wrote_at_ms: u64,
	tuwunel_version: &'static str,
	scope: &'static str,
}

impl DumpMeta {
	fn new(scope: &'static str) -> Self {
		Self {
			pid: process::id(),
			wrote_at_ms: now_millis(),
			tuwunel_version: version(),
			scope,
		}
	}
}

#[cfg(tokio_unstable)]
/// Writes a runtime metrics snapshot into a process-specific JSON file.
///
/// The output filename includes the current process identifier. Serialization
/// and file-system failures are reported through logging instead of being
/// returned.
pub fn write_runtime_metrics(dir: &Path, metrics: &RuntimeMetrics) {
	let pid = process::id();
	let path = dir.join(format!("{RUNTIME_METRICS_PREFIX}.{pid}.json"));
	let payload = format!("{metrics:?}");
	let dump = Dump {
		meta: DumpMeta::new("runtime_metrics"),
		payload: &payload,
	};

	report(&path, "runtime_metrics", write_json(&path, &dump));
}

/// Writes a resource usage snapshot into a process-specific JSON file.
///
/// The output filename includes the current process identifier. Serialization
/// and file-system failures are reported through logging instead of being
/// returned.
pub fn write_resource_usage(dir: &Path, usage: &Usage) {
	let pid = process::id();
	let path = dir.join(format!("{RUNTIME_USAGE_PREFIX}.{pid}.json"));
	let payload = format!("{usage:?}");
	let dump = Dump {
		meta: DumpMeta::new("runtime_usage"),
		payload: &payload,
	};

	report(&path, "runtime_usage", write_json(&path, &dump));
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result {
	if let Some(parent) = path.parent()
		&& !parent.as_os_str().is_empty()
	{
		fs::create_dir_all(parent)?;
	}

	let json = serde_json::to_string_pretty(value)?;
	fs::write(path, json)?;

	Ok(())
}

fn report(path: &Path, scope: &'static str, result: Result) {
	match result {
		| Ok(()) => debug_info!(?path, %scope, "Wrote metrics."),
		| Err(error) => error!(?path, %scope, %error, "Failed to write metrics."),
	}
}
