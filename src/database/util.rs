use rocksdb::{Direction, ErrorKind, IteratorMode};
use tuwunel_core::Result;

#[inline]
pub(crate) fn _into_direction(mode: &IteratorMode<'_>) -> Direction {
	use Direction::{Forward, Reverse};
	use IteratorMode::{End, From, Start};

	match mode {
		| Start | From(_, Forward) => Forward,
		| End | From(_, Reverse) => Reverse,
	}
}

#[inline]
pub(crate) fn result<T>(
	r: std::result::Result<T, rocksdb::Error>,
) -> Result<T, tuwunel_core::Error> {
	r.map_or_else(or_else, and_then)
}

#[inline(always)]
pub(crate) fn and_then<T>(t: T) -> Result<T, tuwunel_core::Error> { Ok(t) }

pub(crate) fn or_else<T>(e: rocksdb::Error) -> Result<T, tuwunel_core::Error> { Err(map_err(e)) }

#[inline]
pub(crate) fn is_incomplete(e: &rocksdb::Error) -> bool { e.kind() == ErrorKind::Incomplete }

pub(crate) fn map_err(e: rocksdb::Error) -> tuwunel_core::Error {
	let kind = io_error_kind(&e.kind());
	let string = e.into_string();

	std::io::Error::new(kind, string).into()
}

fn io_error_kind(e: &ErrorKind) -> std::io::ErrorKind {
	use std::io;

	match e {
		| ErrorKind::NotFound => io::ErrorKind::NotFound,
		| ErrorKind::Corruption => io::ErrorKind::InvalidData,
		| ErrorKind::InvalidArgument => io::ErrorKind::InvalidInput,
		| ErrorKind::Aborted => io::ErrorKind::Interrupted,
		| ErrorKind::NotSupported => io::ErrorKind::Unsupported,
		| ErrorKind::CompactionTooLarge => io::ErrorKind::FileTooLarge,
		| ErrorKind::MergeInProgress | ErrorKind::Busy => io::ErrorKind::ResourceBusy,
		| ErrorKind::Expired | ErrorKind::TimedOut => io::ErrorKind::TimedOut,
		| ErrorKind::Incomplete | ErrorKind::TryAgain => io::ErrorKind::WouldBlock,
		| ErrorKind::ColumnFamilyDropped
		| ErrorKind::ShutdownInProgress
		| ErrorKind::IOError
		| ErrorKind::Unknown => io::ErrorKind::Other,
	}
}
