use futures::{Stream, TryStreamExt};
use tuwunel_core::{Result, Server, utils::TryReadyExt};

/// Bounds a migration's row scan by shutdown and reports its position.
///
/// After a stop request every row becomes an interrupted error, which ends a
/// short-circuiting consumer such as `try_fold` at the next row. Each row read
/// while the server runs advances startup progress.
pub(super) trait ScanExt<T>: Stream<Item = Result<T>> + Send + Sized {
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
	}
}
