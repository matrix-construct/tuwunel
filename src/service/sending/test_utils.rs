use tuwunel_core::{Result, config::Figment};

pub(super) use crate::test_utils::pdu_id;
use crate::test_utils::{Fixture, fixture as service_fixture};

pub(super) async fn fixture(netburst: bool, keep: i64) -> Result<Option<Fixture>> {
	let config = Figment::new()
		.merge(("startup_netburst", netburst))
		.merge(("startup_netburst_keep", keep))
		.merge(("sender_workers", 1));

	service_fixture(config).await
}
