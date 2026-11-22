use ruma::{EventId, RoomId, RoomVersionId, ServerName};

/// One event's evaluation in the federation receive pipeline.
///
/// `event_id` names the event being evaluated. An evaluation the event leads to
/// copies the rest: a previous or missing event keeps the recursion level, and
/// an event fetched with its auth chain goes one level deeper with its auth
/// events known.
#[derive(Clone, Copy)]
pub(super) struct Eval<'a> {
	pub(super) origin: &'a ServerName,
	pub(super) room_id: &'a RoomId,
	pub(super) event_id: &'a EventId,
	pub(super) room_version: &'a RoomVersionId,
	pub(super) create_event_id: &'a EventId,
	pub(super) recursion_level: usize,

	/// Whether the event's auth events were handled before it, so its admission
	/// fetches none.
	pub(super) auth_events_known: bool,
}
