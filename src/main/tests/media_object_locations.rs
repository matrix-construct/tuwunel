#![cfg(all(test, feature = "synapse_admin"))]

use std::{
	net::TcpListener,
	sync::{
		Arc, Mutex,
		atomic::{AtomicBool, AtomicUsize, Ordering},
	},
	time::Duration,
};

use axum::{
	Router,
	extract::State,
	http::{Method, StatusCode, Uri},
	response::IntoResponse,
	routing::any,
};
use serde_json::{Value, json};
use tuwunel_core::{Result, ruma::Mxc};
use tuwunel_service::Services;

use self::{
	client::{Client, register},
	fixture::boot,
};

#[expect(dead_code)] // This test uses only account registration and raw POSTs.
mod client;
mod fixture;

const ADMIN: &str = "object-locations-admin-access-token-for-test";
const USER: &str = "object-locations-user-access-token-for-test";
const ENDPOINT: &str = "/_tuwunel/admin/v1/media/object-locations";

#[derive(Default)]
struct Requests {
	heads: Mutex<Vec<String>>,
	payloads: AtomicUsize,
	active: AtomicUsize,
	maximum: AtomicUsize,
	stall: AtomicBool,
}

async fn head(
	State(requests): State<Arc<Requests>>,
	method: Method,
	uri: Uri,
) -> impl IntoResponse {
	if method != Method::HEAD {
		requests.payloads.fetch_add(1, Ordering::SeqCst);
		return StatusCode::METHOD_NOT_ALLOWED.into_response();
	}
	requests
		.heads
		.lock()
		.expect("request log must not be poisoned")
		.push(uri.path().to_owned());
	let active = requests
		.active
		.fetch_add(1, Ordering::SeqCst)
		.saturating_add(1);
	requests
		.maximum
		.fetch_max(active, Ordering::SeqCst);
	let delay = if requests.stall.load(Ordering::SeqCst) {
		Duration::from_secs(30)
	} else {
		Duration::from_millis(5)
	};
	tokio::time::sleep(delay).await;
	requests.active.fetch_sub(1, Ordering::SeqCst);
	if uri.path().starts_with("/missing/") {
		return StatusCode::NOT_FOUND.into_response();
	}
	if uri.path().starts_with("/denied/") {
		return StatusCode::FORBIDDEN.into_response();
	}
	(
		[
			("content-length", "37"),
			("etag", "\"object-locations-test\""),
			("last-modified", "Wed, 21 Oct 2015 07:28:00 GMT"),
		],
		"",
	)
		.into_response()
}

async fn exercise(services: &Services, base: &str, requests: &Requests) -> Result {
	let admin_id = register(services, "locationadmin", ADMIN).await?;
	register(services, "locationuser", USER).await?;
	services.admin.make_user_admin(&admin_id).await?;
	let admin = Client { services, base, token: ADMIN };
	let user = Client { services, base, token: USER };
	let anonymous = Client { services, base, token: "" };
	let invalid = Client { services, base, token: "invalid-token" };
	let mxc = Mxc {
		server_name: services.globals.server_name(),
		media_id: "original",
	};
	services
		.media
		.create(&mxc, Some(&admin_id), None, Some("text/plain"), b"original")
		.await?;
	let uri = mxc.to_string();
	let query = json!({ "mxc_uris": [&uri] });

	for (caller, status) in [(&user, 403), (&anonymous, 401), (&invalid, 401)] {
		ask(caller, &query, status).await?;
		// Authorization precedes semantic body validation.
		ask(caller, &json!({ "mxc_uris": 42 }), status).await?;
	}
	assert!(
		requests
			.heads
			.lock()
			.expect("request log must not be poisoned")
			.is_empty(),
		"unauthorized requests must not reach storage"
	);

	verified_providers(&admin, &uri, requests).await?;
	provider_selection(&admin, &uri, requests).await?;
	cached_remote(&admin).await?;
	request_bounds(&admin, &uri).await?;
	assert_eq!(
		requests.payloads.load(Ordering::SeqCst),
		0,
		"location lookups must never fetch payloads"
	);
	concurrent_batches(&admin, requests).await?;
	stalled_batches(&admin, requests).await?;
	Ok(())
}

async fn verified_providers(admin: &Client<'_>, uri: &str, requests: &Requests) -> Result {
	let services = admin.services;
	let reply = ask(
		admin,
		&json!({ "mxc_uris": [&uri, &uri, "invalid", "mxc://absent.invalid/none"] }),
		200,
	)
	.await?;
	let locations = reply["locations"]
		.as_array()
		.expect("response array");
	assert_eq!(locations.len(), 3, "duplicate URIs must collapse in first-occurrence order");
	assert_eq!(locations[0]["status"], "found", "present cloud copies must yield found");
	assert_eq!(
		locations[1]["status"], "invalid_mxc",
		"an invalid URI must fail only its own item"
	);
	assert_eq!(locations[2]["status"], "not_found", "an absent remote URI must remain absent");
	assert_eq!(
		locations[2]["objects"],
		json!([]),
		"missing original metadata must not produce candidates"
	);
	let objects = locations[0]["objects"]
		.as_array()
		.expect("response array");
	assert_eq!(objects.len(), 6, "every selected provider must have a result");
	let object = |id: &str| {
		objects
			.iter()
			.find(|o| o["provider"] == id)
			.expect("selected provider result")
	};
	assert_eq!(
		object("source")["bucket"],
		"resolved-source",
		"the built S3 client's URL bucket must override raw bucket settings"
	);
	assert_eq!(object("source")["size"], 37, "verified size must come from HEAD metadata");
	assert_eq!(object("replica")["presence"], "present", "a second replica must remain visible");
	assert_eq!(
		object("missing")["presence"],
		"not_present",
		"missing objects must remain distinct from provider failures"
	);
	assert_eq!(
		object("denied")["presence"],
		"provider_unavailable",
		"access denial must remain an item-level provider failure"
	);
	assert_eq!(
		object("disabled")["presence"],
		"provider_unavailable",
		"disabled providers must remain visible"
	);
	assert_eq!(
		object("media")["presence"],
		"unsupported_backend",
		"local storage cannot supply an object-store transfer locator"
	);
	assert!(
		object("media").get("object_key").is_none(),
		"local filesystem object keys must not be exposed"
	);
	assert!(
		object("media").get("bucket").is_none(),
		"local filesystem roots must not be exposed"
	);
	let key = object("source")["object_key"]
		.as_str()
		.expect("response string");
	let logical = key
		.strip_prefix("media/prefix/")
		.expect("configured object prefix");
	assert_eq!(logical.len(), 43, "the opaque logical key must match Tuwunel's SHA-256 name");
	assert_eq!(
		services
			.storage
			.provider("media")?
			.head(logical)
			.await?
			.size,
		8,
		"the exported key must address the existing original"
	);
	assert!(
		requests
			.heads
			.lock()
			.expect("request log must not be poisoned")
			.iter()
			.any(|path| path == &format!("/resolved-source/{key}")),
		"HEAD must use exactly the exported bucket and object key"
	);

	Ok(())
}

async fn provider_selection(admin: &Client<'_>, uri: &str, requests: &Requests) -> Result {
	let before = requests
		.heads
		.lock()
		.expect("request log must not be poisoned")
		.len();
	assert_eq!(before, 4, "one HEAD per cloud provider, after URI deduplication");
	let unverified = ask(
		admin,
		&json!({ "mxc_uris": [&uri], "providers": ["source", "gcs", "source"], "verify": false }),
		200,
	)
	.await?;
	assert_eq!(
		requests
			.heads
			.lock()
			.expect("request log must not be poisoned")
			.len(),
		before,
		"verify=false for a local MXC must not contact storage"
	);
	assert_eq!(
		unverified["locations"][0]["status"], "unverified",
		"a candidate location must not claim confirmed presence"
	);
	assert_eq!(
		unverified["locations"][0]["objects"]
			.as_array()
			.expect("response array")
			.len(),
		2,
		"duplicate provider IDs must collapse without dropping other providers"
	);
	assert_eq!(
		unverified["locations"][0]["objects"][1]["backend"], "gcs",
		"GCS candidates must identify their backend"
	);
	assert_eq!(
		unverified["locations"][0]["objects"][1]["bucket"], "gcs-media",
		"GCS candidates must identify their configured bucket"
	);
	assert!(
		unverified["locations"][0]["objects"][1]["object_key"]
			.as_str()
			.expect("response string")
			.starts_with("gcs-prefix/"),
		"GCS candidates must include the normalized prefix"
	);

	for (providers, status) in [
		(json!(["missing"]), "not_present"),
		(json!(["missing", "replica"]), "found"),
		(json!(["unknown", "denied"]), "provider_unavailable"),
		(json!(["media"]), "unsupported_backend"),
	] {
		let reply =
			ask(admin, &json!({ "mxc_uris": [&uri], "providers": providers }), 200).await?;
		assert_eq!(
			reply["locations"][0]["status"], status,
			"summary status must reflect all selected providers"
		);
	}

	Ok(())
}

async fn cached_remote(admin: &Client<'_>) -> Result {
	let services = admin.services;
	let remote = Mxc {
		server_name: "cached.invalid".try_into()?,
		media_id: "cached",
	};
	services
		.media
		.create(&remote, None, None, Some("text/plain"), b"remote")
		.await?;
	let reply = ask(admin, &json!({ "mxc_uris": [remote.to_string()], "providers": ["source", "missing"], "verify": false }), 200).await?;
	assert_eq!(
		reply["locations"][0]["objects"][0]["presence"], "present",
		"cached remote locations require verification even with verify=false"
	);
	assert_eq!(
		reply["locations"][0]["objects"][1]["presence"], "not_present",
		"absent remote copies must remain marked missing"
	);
	assert!(
		reply["locations"][0]["objects"][1]
			.get("object_key")
			.is_none(),
		"absent remote copies must not expose speculative locators"
	);

	Ok(())
}

async fn request_bounds(admin: &Client<'_>, uri: &str) -> Result {
	let maximum = ask(
		admin,
		&json!({ "mxc_uris": vec![uri; 1_000], "providers": ["source"], "verify": false }),
		200,
	)
	.await?;
	assert_eq!(
		maximum["locations"]
			.as_array()
			.expect("response array")
			.len(),
		1,
		"the maximum request length must be accepted and deduplicated"
	);
	for body in [
		json!({}),
		json!({ "mxc_uris": [] }),
		json!({ "mxc_uris": vec![&uri; 1_001] }),
		json!({ "mxc_uris": [&uri], "providers": [] }),
		json!({ "mxc_uris": [&uri], "providers": vec!["source"; 101] }),
	] {
		ask(admin, &body, 400).await?;
	}
	Ok(())
}

async fn concurrent_batches(admin: &Client<'_>, requests: &Requests) -> Result {
	let mut uris = Vec::new();
	for i in 0..32 {
		let media_id = format!("concurrent{i}");
		let mxc = Mxc {
			server_name: admin.services.globals.server_name(),
			media_id: &media_id,
		};
		admin
			.services
			.media
			.create(&mxc, None, None, Some("text/plain"), b"concurrent")
			.await?;
		uris.push(mxc.to_string());
	}
	let query = json!({ "mxc_uris": uris, "providers": ["source"] });
	let (first, second) =
		futures::future::try_join(ask(admin, &query, 200), ask(admin, &query, 200)).await?;
	assert_eq!(
		first["locations"]
			.as_array()
			.expect("response array")
			.len(),
		32,
		"concurrent batches must resolve every requested original"
	);
	assert_eq!(first, second, "concurrent batches must preserve deterministic result ordering");
	let maximum = requests.maximum.load(Ordering::SeqCst);
	assert!(maximum > 1, "verification should run concurrently");
	assert!(maximum <= 16, "HEAD concurrency exceeded its global bound: {maximum}");
	assert_eq!(
		requests.payloads.load(Ordering::SeqCst),
		0,
		"concurrent verification must never fetch payloads"
	);
	Ok(())
}

async fn ask(client: &Client<'_>, body: &Value, expected: u16) -> Result<Value> {
	let response = client
		.post_url(&format!("{}{ENDPOINT}", client.base), body)
		.await?;
	let status = response.status().as_u16();
	if status == 200 {
		assert_eq!(
			response.headers()["cache-control"],
			"no-store",
			"administrative object locators must not be cached"
		);
	}
	let body = response.text().await?;
	assert_eq!(status, expected, "{body}");
	Ok(serde_json::from_str(&body)?)
}

/// Both active HEADs and requests waiting for permits must finish before the
/// HTTP timeout; later batches must be able to reuse the released permits.
async fn stalled_batches(admin: &Client<'_>, requests: &Requests) -> Result {
	let uris: Vec<_> = (0..32)
		.map(|i| format!("mxc://{}/concurrent{i}", admin.services.globals.server_name()))
		.collect();
	let query = json!({ "mxc_uris": uris, "providers": ["source", "replica"] });
	requests.stall.store(true, Ordering::SeqCst);
	let (first, second) =
		futures::future::try_join(ask(admin, &query, 200), ask(admin, &query, 200)).await?;
	requests.stall.store(false, Ordering::SeqCst);
	for response in [first, second] {
		let locations = response["locations"]
			.as_array()
			.expect("response array");
		assert_eq!(locations.len(), 32, "a provider timeout must retain every item");
		for location in locations {
			assert_eq!(
				location["status"], "provider_unavailable",
				"a timeout must remain an item-level failure"
			);
			let objects = location["objects"]
				.as_array()
				.expect("provider results");
			assert_eq!(objects.len(), 2, "every selected provider needs a result after expiry");
			assert!(
				objects
					.iter()
					.all(|object| object["presence"] == "provider_unavailable"),
				"a timeout must not claim an object is absent"
			);
		}
	}
	let recovered =
		ask(admin, &json!({ "mxc_uris": [&uris[0]], "providers": ["source"] }), 200).await?;
	assert_eq!(
		recovered["locations"][0]["status"], "found",
		"canceled checks must release their concurrency permits"
	);
	Ok(())
}

/// Exercises real authentication, metadata resolution and S3 HEAD requests.
/// The mock refuses payload operations, isolating verification from downloads.
#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn media_object_locations() -> Result {
		let listener = TcpListener::bind(("127.0.0.1", 0))?;
		let address = listener.local_addr()?;
		listener.set_nonblocking(true)?;
		let mut options = vec![
			"client_request_timeout=4".to_owned(),
			"create_admin_room=true".to_owned(),
			"grant_admin_to_first_user=false".to_owned(),
			"media_startup_check=false".to_owned(),
			"media_storage_providers=[\"media\",\"source\",\"replica\",\"missing\",\"denied\",\"\
			 disabled\"]"
				.to_owned(),
			"store_media_on_providers=[\"media\"]".to_owned(),
			"storage_provider.disabled.s3.startup_check=false".to_owned(),
			"storage_provider.gcs.gcs.bucket=\"gcs-media\"".to_owned(),
			"storage_provider.gcs.gcs.base_path=\"/gcs-prefix/\"".to_owned(),
			"storage_provider.gcs.gcs.use_signatures=false".to_owned(),
			"storage_provider.gcs.gcs.startup_check=false".to_owned(),
		];
		for id in ["source", "replica", "missing", "denied"] {
			options.extend([
				format!("storage_provider.{id}.s3.bucket=\"{id}\""),
				format!("storage_provider.{id}.s3.endpoint=\"http://{address}\""),
				format!("storage_provider.{id}.s3.region=\"us-east-1\""),
				format!("storage_provider.{id}.s3.base_path=\"/media/prefix/\""),
				format!("storage_provider.{id}.s3.use_https=false"),
				format!("storage_provider.{id}.s3.use_signatures=false"),
				format!("storage_provider.{id}.s3.use_vhost_request=false"),
				format!("storage_provider.{id}.s3.startup_check=false"),
			]);
		}
		// The built client's URL overrides its bucket setting; the URL path is
		// ignored by object_store, while base_path supplies the actual prefix.
		options
			.push("storage_provider.source.s3.url=\"s3://resolved-source/ignored\"".to_owned());

		boot("media-object-locations", options, async move |services, base| {
			let requests = Arc::new(Requests::default());
			let router = Router::new()
				.fallback(any(head))
				.with_state(requests.clone());
			let listener = tokio::net::TcpListener::from_std(listener)?;
			let mock = tokio::spawn(async move { axum::serve(listener, router).await });
			let result = exercise(services, base, &requests).await;

			mock.abort();
			result
		})
	}
}
