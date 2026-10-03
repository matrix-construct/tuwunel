use ruma::UserId;
use tuwunel_service::{Services, rooms::timeline::PdusIterItem};

pub(super) mod account;
pub(super) mod account_data;
/// Provides administrator endpoint handlers.
///
/// Its public modules expose the administrator and Matrix Authentication Service route builders.
pub mod admin;
pub(super) mod alias;
pub(super) mod appservice;
pub(super) mod backup;
pub(super) mod capabilities;
pub(super) mod context;
pub(super) mod dehydrated_device;
pub(super) mod device;
pub(super) mod directory;
pub(super) mod events;
pub(super) mod filter;
pub(super) mod keys;
pub(super) mod media;
pub(super) mod media_legacy;
pub(super) mod membership;
pub(super) mod message;
pub(super) mod notice;
pub(super) mod openid;
pub(super) mod presence;
pub(super) mod profile;
pub(super) mod push;
pub(super) mod read_marker;
pub(super) mod redact;
pub(super) mod register;
pub(super) mod relations;
pub(super) mod rendezvous;
pub(super) mod report;
pub(super) mod room;
/// Provides the client route builder.
///
/// It assembles client endpoints using the server configuration.
pub mod routes;
pub(super) mod rtc;
pub(super) mod search;
pub(super) mod send;
pub(super) mod session;
pub(super) mod space;
pub(super) mod state;
pub(super) mod sync;
pub(super) mod tag;
pub(super) mod thirdparty;
pub(super) mod threads;
pub(super) mod to_device;
pub(super) mod tuwunel;
pub(super) mod typing;
pub(super) mod unstable;
pub(super) mod user_directory;
pub(super) mod versions;
pub(super) mod voip;
pub(super) mod well_known;

mod utils;

pub(super) use account::*;
pub(super) use account_data::*;
pub(super) use admin::*;
pub(super) use alias::*;
pub(super) use appservice::*;
pub(super) use backup::*;
pub(super) use capabilities::*;
pub(super) use context::*;
pub(super) use dehydrated_device::*;
pub(super) use device::*;
pub(super) use directory::*;
pub(super) use events::*;
pub(super) use filter::*;
pub(super) use keys::*;
pub(super) use media::*;
pub(super) use media_legacy::*;
pub(super) use membership::*;
pub(super) use message::*;
pub(super) use openid::*;
pub(super) use presence::*;
pub(super) use profile::*;
pub(super) use push::*;
pub(super) use read_marker::*;
pub(super) use redact::*;
pub(super) use register::*;
pub(super) use relations::*;
pub(super) use rendezvous::*;
pub(super) use report::*;
pub(super) use room::*;
pub(super) use rtc::*;
pub(super) use search::*;
pub(super) use send::*;
pub(super) use session::*;
pub(super) use space::*;
pub(super) use state::*;
pub(super) use sync::*;
pub(super) use tag::*;
pub(super) use thirdparty::*;
pub(super) use threads::*;
pub(super) use to_device::*;
pub(super) use tuwunel::*;
pub(super) use typing::*;
pub(super) use unstable::*;
pub(super) use user_directory::*;
pub(super) use versions::*;
pub(super) use voip::*;
pub(super) use well_known::*;

/// generated user access token length
const TOKEN_LENGTH: usize = tuwunel_service::users::device::TOKEN_LENGTH;

/// generated user session ID length
const SESSION_ID_LENGTH: usize = tuwunel_service::uiaa::SESSION_ID_LENGTH;

/// Keeps a timeline item only when the user may see its event.
///
/// The room's history visibility is read as it stood at that event rather than
/// as it stands at the time of the request.
#[inline]
async fn visibility_filter(
	services: &Services,
	item: PdusIterItem,
	user_id: &UserId,
) -> Option<PdusIterItem> {
	let (_, pdu) = &item;

	services
		.state_accessor
		.user_can_see_event(user_id, pdu)
		.await
		.then_some(item)
}
