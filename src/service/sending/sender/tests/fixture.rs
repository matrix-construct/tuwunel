use tuwunel_core::{Result, config::Figment};

use crate::test_utils::{Fixture, fixture as service_fixture};

pub(super) async fn fixture(netburst: bool, keep: i64) -> Result<Option<Fixture>> {
	let config = Figment::new()
		.merge(("sending.startup_netburst", netburst))
		.merge(("sending.startup_netburst_keep", keep))
		.merge(("sending.workers", 1));

	service_fixture(config).await
}
