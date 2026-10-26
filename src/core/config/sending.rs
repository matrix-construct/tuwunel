//! Configures outbound delivery workers and retries.
//!
//! The queue serves federation, appservice, and push destinations.
//! Timeouts, retry pacing, and startup bursting control delivery work.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::true_fn;

/// Configures outbound delivery workers and retries.
///
/// The queue serves federation, appservice, and push destinations.
/// Timeouts, retry pacing, and startup bursting control delivery work.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.sending",
	group = "true"
)]
pub struct Sending {
	/// Sets the federation sender request timeout in seconds.
	///
	/// default: 180
	/// legacy: sender_timeout
	#[serde(default = "default_timeout")]
	pub timeout: u64,

	/// Sets the federation sender idle connection pool timeout (seconds).
	///
	/// default: 180
	/// legacy: sender_idle_timeout
	#[serde(default = "default_idle_timeout")]
	pub idle_timeout: u64,

	/// Sets the federation sender transaction retry backoff limit (seconds).
	///
	/// reloadable: yes
	/// default: 86400
	/// legacy: sender_retry_backoff_limit
	#[serde(default = "default_retry_backoff_limit")]
	pub retry_backoff_limit: u64,

	/// Sets the grace period in seconds before retrying a federation
	/// destination that has failed exactly once.
	///
	/// This replaces the quadratic backoff curve for the first retry, so a
	/// single transient failure does not hold delivery until the next backoff
	/// window. A second consecutive failure returns to the backoff curve. Set
	/// to 0 to disable the grace and back off from the first failure.
	///
	/// default: 15
	/// legacy: sender_retry_grace
	#[serde(default = "default_retry_grace")]
	pub retry_grace: u64,

	/// Sets the grace period for clean shutdown of federation requests
	/// (seconds).
	///
	/// reloadable: yes
	/// default: 5
	/// legacy: sender_shutdown_timeout
	#[serde(default = "default_shutdown_timeout")]
	pub shutdown_timeout: u64,

	/// Retries failed and incomplete messages to remote servers immediately at
	/// startup.
	///
	/// This is called bursting; disabling it retries queued messages on a
	/// paced timer after startup instead of all at once, reducing startup load
	/// when resources are extremely limited or deployment scale is huge.
	///
	/// legacy: startup_netburst
	#[serde(default = "true_fn")]
	pub startup_netburst: bool,

	/// Controls startup message retention when `sending.startup_netburst` is
	/// enabled.
	///
	/// Dropped messages are not reattempted, and this setting has no effect
	/// without startup bursting; -1 retries every message without trimming
	/// queues and can
	/// consume significant disk, while 0 drops all messages without any
	/// redelivery attempt.
	///
	/// default: 50
	/// legacy: startup_netburst_keep
	#[serde(default = "default_startup_netburst_keep")]
	pub startup_netburst_keep: i64,

	/// Sets the number of sender task workers and their parallelism.
	///
	/// The default 0 selects the count internally, likely matching Tokio
	/// worker threads or CPU cores. Set a nonzero value to override that
	/// count.
	///
	/// default: 0
	/// legacy: sender_workers
	#[serde(default)]
	pub workers: usize,
}

impl Default for Sending {
	fn default() -> Self {
		Self {
			timeout: default_timeout(),
			idle_timeout: default_idle_timeout(),
			retry_backoff_limit: default_retry_backoff_limit(),
			retry_grace: default_retry_grace(),
			shutdown_timeout: default_shutdown_timeout(),
			startup_netburst: true_fn(),
			startup_netburst_keep: default_startup_netburst_keep(),
			workers: Default::default(),
		}
	}
}

fn default_timeout() -> u64 { 180 }

fn default_idle_timeout() -> u64 { 180 }

fn default_retry_backoff_limit() -> u64 { 86400 }

fn default_retry_grace() -> u64 { 15 }

fn default_shutdown_timeout() -> u64 { 5 }

fn default_startup_netburst_keep() -> i64 { 50 }
