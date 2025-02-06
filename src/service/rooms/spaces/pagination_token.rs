use std::{
	fmt::{Display, Formatter},
	str::FromStr,
};

use conduwuit::{Error, Result};
use ruma::{api::client::error::ErrorKind, UInt};

use crate::rooms::short::ShortRoomId;

// TODO: perhaps use some better form of token rather than just room count
#[derive(Debug, Eq, PartialEq)]
pub struct PaginationToken {
	/// Path down the hierarchy of the room to start the response at,
	/// excluding the root space.
	pub short_room_ids: Vec<ShortRoomId>,
	pub limit: UInt,
	pub max_depth: UInt,
	pub suggested_only: bool,
}

impl FromStr for PaginationToken {
	type Err = Error;

	fn from_str(value: &str) -> Result<Self> {
		let mut values = value.split('_');
		let mut pag_tok = || {
			let short_room_ids = values
				.next()?
				.split(',')
				.filter_map(|room_s| u64::from_str(room_s).ok())
				.collect();

			let limit = UInt::from_str(values.next()?).ok()?;
			let max_depth = UInt::from_str(values.next()?).ok()?;
			let slice = values.next()?;
			let suggested_only = if values.next().is_none() {
				if slice == "true" {
					true
				} else if slice == "false" {
					false
				} else {
					None?
				}
			} else {
				None?
			};

			Some(Self {
				short_room_ids,
				limit,
				max_depth,
				suggested_only,
			})
		};

		if let Some(token) = pag_tok() {
			Ok(token)
		} else {
			Err(Error::BadRequest(ErrorKind::InvalidParam, "invalid token"))
		}
	}
}

impl Display for PaginationToken {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		let short_room_ids = self
			.short_room_ids
			.iter()
			.map(ToString::to_string)
			.collect::<Vec<_>>()
			.join(",");

		write!(f, "{short_room_ids}_{}_{}_{}", self.limit, self.max_depth, self.suggested_only)
	}
}
