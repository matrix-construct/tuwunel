use ruma::{
	OwnedUserId, RoomId,
	api::client::sync::sync_events::v3::{State as RoomState, StateEvents},
};
use tuwunel_core::{Error, debug_error, err};
use tuwunel_matrix::pdu::PduEvent;
use tuwunel_service::rooms::{lazy_loading::Witness, short::ShortStateHash};

pub use self::state::calculate_heroes;
pub(crate) use self::sync_events::sync_events_route;

mod joined;
mod left;
mod state;
mod sync_events;

#[cfg(test)]
mod tests;

#[derive(Default)]
struct StateChanges {
	heroes: Option<Vec<OwnedUserId>>,
	joined_member_count: Option<u64>,
	invited_member_count: Option<u64>,
	state_events: Vec<PduEvent>,

	/// Witnessed members' current state, lazily loaded for display; never a
	/// membership change, so the device-list scan leaves them out.
	lazy_members: Vec<PduEvent>,
}

struct StateChangeParams<'a> {
	full_state: bool,
	state_after: StateAfter,
	since_shortstatehash: Option<ShortStateHash>,
	horizon_shortstatehash: Option<ShortStateHash>,
	after_shortstatehash: Option<ShortStateHash>,
	current_shortstatehash: ShortStateHash,
	joined_since_last_sync: bool,
	witness: Option<&'a Witness>,
	include_heroes: bool,
}

/// MSC4222 `state_after` opt-in: which room-state field the response carries.
#[derive(Clone, Copy, Debug)]
enum StateAfter {
	Off,
	Stable,
	Unstable,
}

impl StateAfter {
	fn requested(self) -> bool { !matches!(self, Self::Off) }

	fn wrap(self, events: StateEvents) -> RoomState {
		match self {
			| Self::Off => RoomState::Before(events),
			| Self::Stable => RoomState::After(events),
			| Self::Unstable => RoomState::AfterUnstable(events),
		}
	}
}

impl From<(bool, bool)> for StateAfter {
	fn from((stable, unstable): (bool, bool)) -> Self {
		// Unstable opt-in wins: such a client reads the unstable field name.
		match (stable, unstable) {
			| (_, true) => Self::Unstable,
			| (true, _) => Self::Stable,
			| _ => Self::Off,
		}
	}
}

// Keep diagnostic macro expansions under the original tracing target.
fn log_future_since(since: Option<u64>, next_batch: u64) {
	debug_error!(?since, next_batch, "received since > next_batch, clamping");
}

fn missing_room_state(room_id: &RoomId) -> Error {
	err!(Database(error!("Room {room_id} has no state")))
}
