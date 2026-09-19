use ruma::EventId;
use serde::Deserialize;
use tuwunel_core::Result;

/// Reads events through a copyable handle to their storage.
///
/// Callers borrow identifiers and choose the fields they need to decode.
/// Implementations apply the caller's missing-event policy.
pub trait FetchEvent: Copy + Send + Sync {
	/// Decodes an event into the requested representation.
	///
	/// Missing-event handling follows the caller's completeness policy.
	fn get<T>(self, event_id: &EventId) -> impl Future<Output = Result<T>> + Send
	where
		T: for<'de> Deserialize<'de> + Send;

	/// Checks whether an event exists without requiring a full decode.
	///
	/// Missing events yield false unless the caller requires completeness.
	fn exists(self, event_id: &EventId) -> impl Future<Output = Result<bool>> + Send;
}
