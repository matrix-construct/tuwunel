#![cfg(test)]

use std::{env, process, sync::Arc};

use tuwunel::{Args, Runtime, Server};
use tuwunel_core::{Err, Result, utils::time::now};
use tuwunel_service::Services;

const PROVIDER_ID: &str = "test_s3";

const ENV_BUCKET: &str = "TUWUNEL_TEST_S3_BUCKET";
const ENV_URL: &str = "TUWUNEL_TEST_S3_URL";
const ENV_REGION: &str = "TUWUNEL_TEST_S3_REGION";
const ENV_KEY: &str = "TUWUNEL_TEST_S3_KEY";
const ENV_SECRET: &str = "TUWUNEL_TEST_S3_SECRET";
const ENV_ENDPOINT: &str = "TUWUNEL_TEST_S3_ENDPOINT";
const ENV_BASE_PATH: &str = "TUWUNEL_TEST_S3_BASE_PATH";
const ENV_USE_HTTPS: &str = "TUWUNEL_TEST_S3_USE_HTTPS";
const ENV_USE_VHOST: &str = "TUWUNEL_TEST_S3_USE_VHOST_REQUEST";
const ENV_USE_SIGNATURES: &str = "TUWUNEL_TEST_S3_USE_SIGNATURES";
const ENV_USE_PAYLOAD_SIGNATURES: &str = "TUWUNEL_TEST_S3_USE_PAYLOAD_SIGNATURES";

#[test]
fn storage_s3_upload_download() -> Result {
	let Some(options) = collect_options() else {
		eprintln!(
			"storage_s3_upload_download: skipped (set {ENV_BUCKET} or {ENV_URL} to enable)"
		);

		return Ok(());
	};

	let mut args = Args::default_test(&["fresh", "cleanup"]);
	args.maintenance = true;
	args.option.extend(options);

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;

	let result: Result = runtime.block_on(async {
		let services = tuwunel::async_start(&server).await?;

		let roundtrip = roundtrip(&services).await;

		server.server.shutdown()?;
		drop(services);

		tuwunel::async_run(&server).await?;
		tuwunel::async_stop(&server).await?;

		roundtrip
	});

	drop(runtime);
	result
}

async fn roundtrip(services: &Arc<Services>) -> Result {
	let provider = services.storage.provider(PROVIDER_ID)?;
	let path = unique_path();
	let payload = b"tuwunel s3 storage roundtrip integration test payload\n".to_vec();

	provider.put_one(&path, payload.clone()).await?;

	let got = provider.get(&path).await?;
	provider.delete_one(&path).await.ok();

	if got.as_ref() != payload.as_slice() {
		return Err!("downloaded bytes did not match uploaded");
	}

	Ok(())
}

fn collect_options() -> Option<Vec<String>> {
	let env_var = |name: &str| env::var(name).ok().filter(|s| !s.is_empty());

	env_var(ENV_BUCKET).or_else(|| env_var(ENV_URL))?;

	let str_field = |field, name| {
		env_var(name).map(|v| {
			let escaped = escape_toml(&v);
			format!("storage_provider.{PROVIDER_ID}.s3.{field}=\"{escaped}\"")
		})
	};

	let raw_field = |field, name| {
		env_var(name).map(|v| format!("storage_provider.{PROVIDER_ID}.s3.{field}={v}"))
	};

	[
		str_field("bucket", ENV_BUCKET),
		str_field("url", ENV_URL),
		str_field("region", ENV_REGION),
		str_field("key", ENV_KEY),
		str_field("secret", ENV_SECRET),
		str_field("endpoint", ENV_ENDPOINT),
		str_field("base_path", ENV_BASE_PATH),
		raw_field("use_https", ENV_USE_HTTPS),
		raw_field("use_vhost_request", ENV_USE_VHOST),
		raw_field("use_signatures", ENV_USE_SIGNATURES),
		raw_field("use_payload_signatures", ENV_USE_PAYLOAD_SIGNATURES),
	]
	.into_iter()
	.flatten()
	.collect::<Vec<_>>()
	.into()
}

fn escape_toml(value: &str) -> String { value.replace('\\', "\\\\").replace('"', "\\\"") }

fn unique_path() -> String {
	let nanos = now().as_nanos();
	let pid = process::id();

	format!("tuwunel-integration-test/{nanos}-{pid}.bin")
}
