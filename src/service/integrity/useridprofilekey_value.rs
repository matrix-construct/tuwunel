//! The values of `useridprofilekey_value`.
//!
//! Values are JSON, unlike the plain text of the columns they were moved from.

use std::{fmt, str::from_utf8};

use futures::Stream;
use serde::de::IgnoredAny;
use tuwunel_core::{Result, smallstr::SmallString, utils::TryReadyExt};
use tuwunel_database::{KeyBuf, keyval::Buffer};

use super::{Check, Report, ScanExt, tally};
use crate::Services;

type Text = SmallString<[u8; 48]>;
type Value = Buffer<48>;

pub(super) static CHECK: Check = Check {
	name: "profile-values",
	about: "Every useridprofilekey_value value is JSON",
	quiescent: false,
	walk: |services, limit| tally(scan(services), limit),
};

/// A `useridprofilekey_value` value that is not JSON.
///
/// Readers of the column decode JSON, so such a field fails to read. Text was
/// stored without its encoding and can recover it, while anything else cannot.
pub enum Finding {
	/// The value is text, stored bare.
	Bare {
		/// The stored key.
		key: KeyBuf,

		/// The stored text.
		text: Text,
	},

	/// The value is neither JSON nor text.
	Opaque {
		/// The stored key.
		key: KeyBuf,

		/// The stored value.
		value: Value,
	},
}

/// Streams each value in `useridprofilekey_value` that is not JSON.
///
/// Only the syntax of a value is judged, not whether it suits its field. The
/// defect is visible in the row alone, so any later row is unaffected by a repair
/// of this one.
#[tracing::instrument(level = "trace", skip_all)]
pub fn scan(services: &Services) -> impl Stream<Item = Result<Finding>> + Send + '_ {
	services.db["useridprofilekey_value"]
		.raw_scan()
		.scanned(&services.server)
		.ready_try_filter_map(|(key, value)| Ok(classify(key, value)))
}

/// Classifies one row as [`scan`] does.
///
/// A repair reads the row a finding names again and passes it back through here
/// before writing, so detection and repair share one predicate.
pub(crate) fn classify(key: &[u8], value: &[u8]) -> Option<Finding> {
	serde_json::from_slice::<IgnoredAny>(value)
		.is_err()
		.then(|| match from_utf8(value) {
			| Ok(text) => Finding::Bare {
				key: KeyBuf::from_slice(key),
				text: text.into(),
			},
			| Err(_) => Finding::Opaque {
				key: KeyBuf::from_slice(key),
				value: Value::from_slice(value),
			},
		})
}

impl Report for Finding {
	fn kind(&self) -> &'static str {
		match self {
			| Self::Bare { .. } => "bare-text",
			| Self::Opaque { .. } => "not-text",
		}
	}
}

impl fmt::Display for Finding {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			| Self::Bare { key, .. } => {
				write!(f, "value of {} is text, not JSON", key.escape_ascii())
			},
			| Self::Opaque { key, .. } => {
				write!(f, "value of {} is neither JSON nor text", key.escape_ascii())
			},
		}
	}
}
