//! Account setup for the tests that declare this module by path.
//!
//! It stands apart from the client harness so that only a binary declaring it
//! compiles it; the harness's other users stay free of its dead code.

use tuwunel_core::{Result, ruma::OwnedUserId};
use tuwunel_service::Services;

use super::client::{Client, register};

/// Register a user and open a client on the token its device holds.
///
/// The token is named once, so the client and the device cannot disagree.
pub(crate) async fn account<'a>(
	services: &'a Services,
	base: &'a str,
	localpart: &str,
	token: &'a str,
) -> Result<(OwnedUserId, Client<'a>)> {
	let user_id = register(services, localpart, token).await?;

	Ok((user_id, Client { services, base, token }))
}
