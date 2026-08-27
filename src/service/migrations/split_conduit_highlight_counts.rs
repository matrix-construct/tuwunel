use tuwunel_core::Result;

use super::{conduit::migrate_conduit_highlight_split, pending};
use crate::Services;

/// Splits a Conduit database's conflated highlight-count column once.
///
/// Conduit aliased `roomuserid_lastnotificationread` onto the
/// `userroomid_highlightcount` tree, so one column holds both stores; tuwunel
/// keeps them apart. Gated on its own marker; the split itself returns early
/// unless a room-keyed row is present, so it is a cheap no-op on a native
/// database.
pub(super) async fn split_conduit_highlight_counts(services: &Services) -> Result {
	if pending(services, "split_conduit_highlight").await? {
		migrate_conduit_highlight_split(services).await?;
		services.db["global"].insert("split_conduit_highlight", []);
	}

	Ok(())
}
