use std::io::{Error as IoError, ErrorKind as IoErrorKind};

use super::{NotFound, Result};
use crate::{Err, Error};

#[test]
fn only_a_not_found_error_is_an_absent_value() {
	let present: Result<u8> = Ok(1);
	let missing: Result<u8> = Err!(Request(NotFound("test value")));
	let failed: Result<u8> = Err!(Database("test failure"));

	assert!(matches!(present.optional(), Ok(Some(1))));
	assert!(matches!(missing.optional(), Ok(None)));
	assert!(matches!(failed.optional(), Err(error) if !error.is_not_found()));
}

#[test]
fn only_a_missing_record_is_an_absent_value() {
	let stored: Result<u8> = Ok(1);
	let missing: Result<u8> = Err!(Request(NotFound("test value")));
	let unreadable: Result<u8> = Err(Error::from(IoError::from(IoErrorKind::NotFound)));

	assert!(matches!(stored.present(), Ok(Some(1))));
	assert!(missing.is_missing());
	assert!(matches!(missing.present(), Ok(None)));
	assert!(unreadable.is_not_found());
	assert!(!unreadable.is_missing());
	assert!(matches!(unreadable.present(), Err(error) if error.is_not_found()));
}
