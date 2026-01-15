use std::{fmt, time::SystemTime};

use futures::{
	Future, FutureExt, TryFutureExt,
	io::{AsyncWriteExt, BufWriter},
	lock::Mutex,
};
use ruma::EventId;
use tuwunel_core::Result;
use tuwunel_service::Services;

pub(crate) struct Context<'a> {
	pub(crate) services: &'a Services,
	pub(crate) body: &'a [&'a str],
	pub(crate) timer: SystemTime,
	pub(crate) reply_id: Option<&'a EventId>,
	pub(crate) output: Mutex<BufWriter<Vec<u8>>>,
}

impl Context<'_> {
	pub(crate) fn write_fmt(
		&self,
		arguments: fmt::Arguments<'_>,
	) -> impl Future<Output = Result> + Send + '_ + use<'_> {
		let buf = format!("{arguments}");
		self.write_string(buf)
	}

	#[inline]
	pub(crate) async fn write_string(&self, s: String) -> Result { self.write_str(&s).await }

	pub(crate) fn write_str<'a>(
		&'a self,
		s: &'a str,
	) -> impl Future<Output = Result> + Send + 'a {
		self.output.lock().then(async move |mut output| {
			output
				.write_all(s.as_bytes())
				.map_err(Into::into)
				.await
		})
	}
}
