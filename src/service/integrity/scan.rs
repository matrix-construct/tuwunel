use futures::{Stream, TryStreamExt};
use tokio::task::consume_budget;
use tuwunel_core::{Result, Server, utils::TryReadyExt};

/// Bounds a row walk by shutdown, reports its position and shares the runtime.
///
/// After a stop request every row becomes an interrupted error, which ends a
/// short-circuiting consumer such as `try_fold` at the next row. Each row read
/// while the server runs advances the progress of any phase in flight. A cursor
/// is ready on every poll after its seek, so each row also spends cooperative
/// budget; otherwise a walk on a serving database would hold its worker for the
/// whole column.
pub(crate) trait ScanExt<T>: Stream<Item = Result<T>> + Send + Sized {
	fn scanned(self, server: &Server) -> impl Stream<Item = Result<T>> + Send;
}

impl<T, S> ScanExt<T> for S
where
	S: Stream<Item = Result<T>> + Send,
	T: Send,
{
	fn scanned(self, server: &Server) -> impl Stream<Item = Result<T>> + Send {
		self.ready_and_then(|row| server.check_running().map(|()| row))
			.inspect_ok(|_| server.progress.advance())
			.and_then(async |row| {
				consume_budget().await;

				Ok(row)
			})
	}
}
