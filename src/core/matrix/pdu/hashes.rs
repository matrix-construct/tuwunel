use serde::{Deserialize, Serialize};

use crate::arrayvec::ArrayString;

/// Content hashes of a PDU.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct EventHashes {
	/// The SHA-256 hash.
	pub sha256: ArrayString<SHA256_LEN>,
}

const SHA256_LEN: usize = 43;
