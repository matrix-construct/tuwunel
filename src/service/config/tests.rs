use std::{fs::write, iter::once, path::Path, sync::Arc};

use tuwunel_core::{Result, config::Figment};

use crate::test_utils::fixture;

#[tokio::test]
async fn rejected_hierarchy_reload_preserves_published_configuration() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let service = &fixture.services.config;
	let initial = service.server.config.clone();
	let path = initial
		.database_path
		.join("hierarchy-reload.toml");

	let header = format!(
		"[global]\nserver_name = {:?}\ndatabase_path = {:?}\nerror_on_unknown_config_opts = \
		 true\n",
		initial.server_name.as_str(),
		initial.database_path,
	);

	write_config(&path, &header, "[global.rocksdb]\nmax_log_files = 17\n")?;
	let replaced = service.reload(once(path.as_path()))?;
	let published = service.server.config.clone();

	assert!(Arc::ptr_eq(&initial, &replaced));
	assert!(!Arc::ptr_eq(&initial, &published));
	assert_eq!(published.rocksdb.max_log_files, 17);

	for (input, key) in [
		(
			"rocksdb_checksums = true\n[global.rocksdb]\nchecksums = false\nmax_log_files = 31\n",
			"rocksdb_checksums",
		),
		("[global.rocksdb]\nchecksums = 'invalid'\nmax_log_files = 31\n", "checksums"),
		(
			"[global.rocksdb]\nunknown_option = true\nmax_log_files = 31\n",
			"rocksdb.unknown_option",
		),
	] {
		write_config(&path, &header, input)?;
		let error = service
			.reload(once(path.as_path()))
			.err()
			.expect("invalid hierarchy must fail reload")
			.to_string();

		assert!(error.contains(key), "{error}");

		let retained = service.server.config.clone();

		assert!(Arc::ptr_eq(&published, &retained), "failure published a replacement");
		assert_eq!(retained.rocksdb.max_log_files, 17);
	}

	write_config(&path, &header, "[global.rocksdb]\nmax_log_files = 29\n")?;
	let replaced = service.reload(once(path.as_path()))?;
	let recovered = service.server.config.clone();

	assert!(Arc::ptr_eq(&published, &replaced));
	assert!(!Arc::ptr_eq(&published, &recovered));
	assert_eq!(recovered.rocksdb.max_log_files, 29);
	assert_eq!(published.rocksdb.max_log_files, 17);

	Ok(())
}

fn write_config(path: &Path, header: &str, body: &str) -> Result {
	write(path, format!("{header}{body}"))?;

	Ok(())
}
