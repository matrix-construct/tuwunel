//! MSC4521 wire endpoints are shared with the outbound service client.

pub(crate) use tuwunel_service::rooms::reconcile::api::{
	diff as post_room_diff, digest as get_room_digest, strata as get_room_digest_strata,
};
