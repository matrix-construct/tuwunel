//! Configures stream concurrency and batching.
//!
//! Stream widths set the amount of concurrent work. Amplification controls
//! request batch sizes independently of the detected hardware queue topology.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

/// Groups runtime stream scheduling settings.
///
/// Stream settings control initial concurrency, hardware scaling, and batch
/// amplification for database work.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.runtime",
	group = "true",
	ignore = "stream"
)]
pub struct Runtime {
	/// Settings for runtime stream.
	///
	/// display: section
	#[serde(default)]
	pub stream: Stream,
}

/// Controls stream concurrency and request batch amplification.
///
/// Width can remain fixed or scale with detected hardware capacity.
/// Amplification adjusts how much work each pool worker receives in a batch.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.runtime.stream",
	group = "true"
)]
pub struct Stream {
	/// Sets the initial stream concurrency.
	///
	/// The default is 32. The computed
	/// `runtime.stream.width_scale` overrides this value unless scaling is
	/// disabled, in which case it provides a fixed stream width.
	///
	/// default: 32
	/// legacy: stream_width_default
	#[serde(default = "default_width_default")]
	pub width_default: usize,

	/// Scales stream concurrency from the detected system capacity.
	///
	/// The base is the database pool worker count derived from hardware
	/// queue size, such as 32 for SSDs or 64 or 128+ for NVMe; multipliers
	/// such as 1.5 or 2.0 increase it, while 0.5 improves responsiveness for
	/// many users at the cost of per-user throughput. The result cannot
	/// exceed the pool queue size set by `database.pool.queue_mult`, because
	/// larger values stall the Tokio task. A value of 0.0 fixes the width at
	/// `runtime.stream.width_default`. The default scale of 1.0 matches
	/// detected system capabilities.
	///
	/// default: 1.0
	/// legacy: stream_width_scale
	#[serde(default = "default_width_scale")]
	pub width_scale: f32,

	/// Sets the initial request batch amplification factor.
	///
	/// The factor controls batch sizes submitted by each pool worker,
	/// multiplying stream throughput independently of specific hardware
	/// characteristics, so it can greatly exceed thread counts or queue
	/// sizes. Each database query may require several index lookups,
	/// allowing queries in a batch to progress independently while sharing
	/// index and data blocks, whether cached or not. Large batches reduce
	/// complexity. The maximum is 32768, which still requires sufficient
	/// hardware.
	///
	/// default: 1024
	/// legacy: stream_amplification
	#[serde(default = "default_amplification")]
	pub amplification: usize,
}

impl Default for Stream {
	fn default() -> Self {
		Self {
			width_default: default_width_default(),
			width_scale: default_width_scale(),
			amplification: default_amplification(),
		}
	}
}

fn default_width_default() -> usize { 32 }

fn default_width_scale() -> f32 { 1.0 }

fn default_amplification() -> usize { 1024 }
