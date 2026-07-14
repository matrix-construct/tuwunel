use std::{fmt, fmt::Debug, time::SystemTime};

use futures::{Future, FutureExt, lock::Mutex};
use tokio::time::Instant;
use tuwunel_core::{Err, Result};

use crate::Services;

/// Ceiling on a single command's accumulated output; a handler that writes past
/// it aborts rather than letting the buffer grow without bound.
const OUTPUT_MAX_BYTES: usize = 64 * 1024 * 1024;

pub struct Context<'a> {
	pub services: &'a Services,
	pub body: &'a [&'a str],
	pub timer: SystemTime,
	pub output: Mutex<String>,
}

impl Context<'_> {
	pub async fn write_timed_query<F, T>(&self, query: F) -> Result
	where
		F: Future<Output = T>,
		T: Debug,
	{
		let timer = Instant::now();
		let result = query.await;
		let query_time = timer.elapsed();

		self.write_string(format!(
			"Query completed in {query_time:?}:\n\n```rs\n{result:#?}\n```"
		))
		.await
	}

	pub async fn write_timed_query_try<F, T>(&self, query: F) -> Result
	where
		F: Future<Output = Result<T>>,
		T: Debug,
	{
		let timer = Instant::now();
		let result = query.await?;
		let query_time = timer.elapsed();

		self.write_string(format!(
			"Query completed in {query_time:?}:\n\n```rs\n{result:#?}\n```"
		))
		.await
	}

	pub fn write_fmt(
		&self,
		arguments: fmt::Arguments<'_>,
	) -> impl Future<Output = Result> + Send + '_ + use<'_> {
		let buf = format!("{arguments}");
		self.write_string(buf)
	}

	#[inline]
	pub async fn write_string(&self, s: String) -> Result { self.write_str(&s).await }

	pub fn write_str<'a>(&'a self, s: &'a str) -> impl Future<Output = Result> + Send + 'a {
		self.output.lock().map(move |mut output| {
			if output.len().saturating_add(s.len()) > OUTPUT_MAX_BYTES {
				return Err!("Command output exceeded the maximum size and was aborted.");
			}

			output.push_str(s);
			Ok(())
		})
	}
}
