use tracing_subscriber::fmt::format::FmtSpan;

use crate::Result;

/// Parses a tracing span-lifecycle mode without case sensitivity.
///
/// Recognized names map to the corresponding `FmtSpan` flag. Unknown names
/// return `FmtSpan::NONE` on the error side for use as a fallback.
#[inline]
pub fn from_str(str: &str) -> Result<FmtSpan, FmtSpan> {
	match str.to_uppercase().as_str() {
		| "ENTER" => Ok(FmtSpan::ENTER),
		| "EXIT" => Ok(FmtSpan::EXIT),
		| "NEW" => Ok(FmtSpan::NEW),
		| "CLOSE" => Ok(FmtSpan::CLOSE),
		| "ACTIVE" => Ok(FmtSpan::ACTIVE),
		| "FULL" => Ok(FmtSpan::FULL),
		| "NONE" => Ok(FmtSpan::NONE),
		| _ => Err(FmtSpan::NONE),
	}
}
