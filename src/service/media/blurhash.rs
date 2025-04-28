#[cfg(feature = "blurhashing")]
use tuwunel_core::config::BlurhashConfig as CoreBlurhashConfig;
use tuwunel_core::{Result, implement};

use super::Service;

#[implement(Service)]
#[cfg(not(feature = "blurhashing"))]
pub fn create_blurhash(
	&self,
	_file: &[u8],
	_content_type: Option<&str>,
	_file_name: Option<&str>,
) -> Result<Option<String>> {
	tuwunel_core::debug_warn!("blurhashing on upload support was not compiled");

	Ok(None)
}

#[implement(Service)]
#[cfg(feature = "blurhashing")]
pub fn create_blurhash(
	&self,
	file: &[u8],
	content_type: Option<&str>,
	file_name: Option<&str>,
) -> Result<Option<String>> {
	let config = BlurhashConfig::from(self.services.server.config.blurhashing);

	// since 0 means disabled blurhashing, skipped blurhashing
	if config.size_limit == 0 {
		return Ok(None);
	}

	get_blurhash_from_request(file, content_type, file_name, config)
		.map_err(|e| tuwunel_core::err!(debug_error!("blurhashing error: {e}")))
		.map(Some)
}

/// Returns the blurhash or a blurhash error which implements Display.
#[tracing::instrument(
	name = "blurhash",
	level = "debug",
	skip(data),
	fields(
		bytes = data.len(),
	),
)]
#[cfg(feature = "blurhashing")]
fn get_blurhash_from_request(
	data: &[u8],
	mime: Option<&str>,
	filename: Option<&str>,
	config: BlurhashConfig,
) -> Result<String, BlurhashingError> {
	// Get format image is supposed to be in
	let format = get_format_from_data_mime_and_filename(data, mime, filename)?;

	// Get the image reader for said image format
	let decoder = get_image_decoder_with_format_and_data(format, data)?;

	// Check image size makes sense before unpacking whole image
	if is_image_above_size_limit(&decoder, config) {
		return Err(BlurhashingError::ImageTooLarge);
	}

	let image = image::DynamicImage::from_decoder(decoder)?;

	blurhash_an_image(&image, config)
}

/// Gets the Image Format value from the data,mime, and filename
/// It first checks if the mime is a valid image format
/// Then it checks if the filename has a format, otherwise just guess based on
/// the binary data Assumes that mime and filename extension won't be for a
/// different file format than file.
#[cfg(feature = "blurhashing")]
fn get_format_from_data_mime_and_filename(
	data: &[u8],
	mime: Option<&str>,
	filename: Option<&str>,
) -> Result<image::ImageFormat, BlurhashingError> {
	let extension = filename
		.map(std::path::Path::new)
		.and_then(std::path::Path::extension)
		.map(std::ffi::OsStr::to_string_lossy);

	mime.or(extension.as_deref())
		.and_then(image::ImageFormat::from_mime_type)
		.map_or_else(|| image::guess_format(data).map_err(Into::into), Ok)
}

#[cfg(feature = "blurhashing")]
fn get_image_decoder_with_format_and_data(
	image_format: image::ImageFormat,
	data: &[u8],
) -> Result<Box<dyn image::ImageDecoder + '_>, BlurhashingError> {
	let mut image_reader = image::ImageReader::new(std::io::Cursor::new(data));
	image_reader.set_format(image_format);
	Ok(Box::new(image_reader.into_decoder()?))
}

#[cfg(feature = "blurhashing")]
fn is_image_above_size_limit<T: image::ImageDecoder>(
	decoder: &T,
	blurhash_config: BlurhashConfig,
) -> bool {
	decoder.total_bytes() >= blurhash_config.size_limit
}

#[cfg(feature = "blurhashing")]
#[tracing::instrument(name = "encode", level = "debug", skip_all)]
#[inline]
fn blurhash_an_image(
	image: &image::DynamicImage,
	blurhash_config: BlurhashConfig,
) -> Result<String, BlurhashingError> {
	Ok(blurhash::encode_image(
		blurhash_config.components_x,
		blurhash_config.components_y,
		&image.to_rgba8(),
	)?)
}

#[derive(Clone, Copy, Debug)]
pub struct BlurhashConfig {
	pub components_x: u32,
	pub components_y: u32,

	/// size limit in bytes
	pub size_limit: u64,
}

#[cfg(feature = "blurhashing")]
impl From<CoreBlurhashConfig> for BlurhashConfig {
	fn from(value: CoreBlurhashConfig) -> Self {
		Self {
			components_x: value.components_x,
			components_y: value.components_y,
			size_limit: value.blurhash_max_raw_size,
		}
	}
}

#[derive(Debug)]
#[cfg(feature = "blurhashing")]
pub enum BlurhashingError {
	HashingLibError(Box<dyn std::error::Error + Send>),
	#[cfg(feature = "blurhashing")]
	ImageError(Box<image::ImageError>),
	ImageTooLarge,
}

#[cfg(feature = "blurhashing")]
impl From<image::ImageError> for BlurhashingError {
	fn from(value: image::ImageError) -> Self {
		Self::ImageError(Box::new(value))
	}
}

#[cfg(feature = "blurhashing")]
impl From<blurhash::Error> for BlurhashingError {
	fn from(value: blurhash::Error) -> Self {
		Self::HashingLibError(Box::new(value))
	}
}

#[cfg(feature = "blurhashing")]
impl std::fmt::Display for BlurhashingError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "Blurhash Error:")?;
		match &self {
			| Self::ImageTooLarge => write!(f, "Image was too large to blurhash")?,
			| Self::HashingLibError(e) => {
				write!(f, "There was an error with the blurhashing library => {e}")?
			},
			#[cfg(feature = "blurhashing")]
			| Self::ImageError(e) => {
				write!(f, "There was an error with the image loading library => {e}")?
			},
		}

		Ok(())
	}
}
