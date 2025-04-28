use std::fmt::Display;

use serde::{de, ser};

use crate::Error;

impl de::Error for Error {
	fn custom<T: Display + ToString>(msg: T) -> Self {
		Self::SerdeDe(msg.to_string().into())
	}
}

impl ser::Error for Error {
	fn custom<T: Display + ToString>(msg: T) -> Self {
		Self::SerdeSer(msg.to_string().into())
	}
}
