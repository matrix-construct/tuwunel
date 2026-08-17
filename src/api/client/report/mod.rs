mod report_event;
mod report_room;
mod report_user;

pub(crate) use self::{
	report_event::report_event_route, report_room::report_room_route,
	report_user::report_user_route,
};

pub(super) const REASON_MAX_LEN: usize = 2000;
