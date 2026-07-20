#![cfg(test)]

use ruma::media::Method;

use super::Dim;

fn scale(width: u32, height: u32) -> Dim { Dim::new(width, height, Some(Method::Scale)) }
fn crop(width: u32, height: u32) -> Dim { Dim::new(width, height, Some(Method::Crop)) }
fn source(width: u32, height: u32) -> Dim { Dim::new(width, height, None) }

/// The source already carries the requested dimensions, so scaling is a no-op
/// and re-encoding would only discard the original's format and metadata.
#[test]
fn passthrough_when_scale_request_matches_source() {
	assert!(
		scale(800, 600)
			.is_passthrough(&source(800, 600))
			.unwrap()
	);
}

/// `scaled` covers the requested box rather than fitting inside it, so a source
/// already meeting the requested height cannot shrink: the generated thumbnail
/// would be the source itself.
#[test]
fn passthrough_when_one_side_already_meets_request() {
	assert!(
		scale(800, 600)
			.is_passthrough(&source(1000, 600))
			.unwrap()
	);
}

/// A genuinely larger source must still be thumbnailed.
#[test]
fn generates_when_source_is_larger_in_both_dimensions() {
	assert!(
		!scale(800, 600)
			.is_passthrough(&source(4000, 3000))
			.unwrap()
	);
}

/// Crop reaches the requested size by cropping, so a source that is larger in
/// only one dimension must still be generated. Widening the upscale guard to
/// `>=` would wrongly pass this through.
#[test]
fn generates_when_crop_source_is_larger_in_one_dimension() {
	assert!(
		!crop(96, 96)
			.is_passthrough(&source(500, 96))
			.unwrap()
	);
}

/// Servers must not upscale; a source smaller than the request is served as-is.
#[test]
fn passthrough_when_request_exceeds_source() {
	assert!(
		crop(96, 96)
			.is_passthrough(&source(50, 50))
			.unwrap()
	);
	assert!(
		scale(800, 600)
			.is_passthrough(&source(1000, 400))
			.unwrap()
	);
}

/// Crop at exactly the source size reproduces the source.
#[test]
fn passthrough_when_crop_request_matches_source() {
	assert!(
		crop(96, 96)
			.is_passthrough(&source(96, 96))
			.unwrap()
	);
}

#[tokio::test]
#[cfg(disable)] //TODO: fixme
async fn long_file_names_works() {
	use std::path::PathBuf;

	use base64::{Engine as _, engine::general_purpose};

	use super::*;

	struct MockedKVDatabase;

	impl Data for MockedKVDatabase {
		fn create_file_metadata(
			&self,
			_sender_user: Option<&str>,
			mxc: String,
			width: u32,
			height: u32,
			content_disposition: Option<&str>,
			content_type: Option<&str>,
		) -> Result<Vec<u8>> {
			// copied from src/database/key_value/media.rs
			let mut key = mxc.as_bytes().to_vec();
			key.push(0xFF);
			key.extend_from_slice(&width.to_be_bytes());
			key.extend_from_slice(&height.to_be_bytes());
			key.push(0xFF);
			key.extend_from_slice(
				content_disposition
					.as_ref()
					.map(|f| f.as_bytes())
					.unwrap_or_default(),
			);
			key.push(0xFF);
			key.extend_from_slice(
				content_type
					.as_ref()
					.map(|c| c.as_bytes())
					.unwrap_or_default(),
			);

			Ok(key)
		}

		fn delete_file_mxc(&self, _mxc: String) -> Result { todo!() }

		fn search_mxc_metadata_prefix(&self, _mxc: String) -> Result<Vec<Vec<u8>>> { todo!() }

		fn get_all_media_keys(&self) -> Vec<Vec<u8>> { todo!() }

		fn search_file_metadata(
			&self,
			_mxc: String,
			_width: u32,
			_height: u32,
		) -> Result<(Option<String>, Option<String>, Vec<u8>)> {
			todo!()
		}

		fn remove_url_preview(&self, _url: &str) -> Result { todo!() }

		fn set_url_preview(
			&self,
			_url: &str,
			_data: &UrlPreviewData,
			_timestamp: std::time::Duration,
		) -> Result {
			todo!()
		}

		fn get_url_preview(&self, _url: &str) -> Option<UrlPreviewData> { todo!() }
	}

	let db: Arc<MockedKVDatabase> = Arc::new(MockedKVDatabase);
	let mxc = "mxc://example.com/ascERGshawAWawugaAcauga".to_owned();
	let width = 100;
	let height = 100;
	let content_disposition = "attachment; filename=\"this is a very long file name with spaces \
	                           and special characters like äöüß and even emoji like 🦀.png\"";
	let content_type = "image/png";
	let key = db
		.create_file_metadata(
			None,
			mxc,
			width,
			height,
			Some(content_disposition),
			Some(content_type),
		)
		.unwrap();
	let mut r = PathBuf::from("/tmp/media");
	// r.push(base64::encode_config(key, base64::URL_SAFE_NO_PAD));
	// use the sha256 hash of the key as the file name instead of the key itself
	// this is because the base64 encoded key can be longer than 255 characters.
	r.push(general_purpose::URL_SAFE_NO_PAD.encode(<sha2::Sha256 as sha2::Digest>::digest(key)));
	// Check that the file path is not longer than 255 characters
	// (255 is the maximum length of a file path on most file systems)
	assert!(
		r.to_str().unwrap().len() <= 255,
		"File path is too long: {}",
		r.to_str().unwrap().len()
	);
}
