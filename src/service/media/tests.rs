#![cfg(test)]

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

		fn delete_file_mxc(&self, _mxc: String) -> Result<()> {
			todo!()
		}

		fn search_mxc_metadata_prefix(&self, _mxc: String) -> Result<Vec<Vec<u8>>> {
			todo!()
		}

		fn get_all_media_keys(&self) -> Vec<Vec<u8>> {
			todo!()
		}

		fn search_file_metadata(
			&self,
			_mxc: String,
			_width: u32,
			_height: u32,
		) -> Result<(Option<String>, Option<String>, Vec<u8>)> {
			todo!()
		}

		fn remove_url_preview(&self, _url: &str) -> Result<()> {
			todo!()
		}

		fn set_url_preview(
			&self,
			_url: &str,
			_data: &UrlPreviewData,
			_timestamp: std::time::Duration,
		) -> Result<()> {
			todo!()
		}

		fn get_url_preview(&self, _url: &str) -> Option<UrlPreviewData> {
			todo!()
		}
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
