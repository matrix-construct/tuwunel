//! Verification-request limits keyed by source and destination.
//!
//! Separate token buckets protect the caller-IP and target-address axes. Their
//! state is process-local, bounded in size, and reset when the service restarts.

use std::{borrow::Borrow, hash::Hash, net::IpAddr, time::Instant};

use http::StatusCode;
use ruma::api::error::{ErrorKind, LimitExceededErrorData};
use tuwunel_core::{Error, Result, implement};

use super::{EmailKey, Ratelimiter};

/// Refills per second on each requestToken bucket; a generous burst absorbs a
/// real client's retries while bounding sustained spray.
const RC_PER_SECOND: f64 = 0.2;
const RC_BURST: f64 = 5.0;

/// Cap on each bucket table; fully refilled buckets are pruned past it so a
/// spray cannot grow the table without bound.
const RATELIMIT_MAP_CAP: usize = 1 << 16;

#[cfg(test)]
mod tests;

/// Applies the verification-request limit for one caller IP address.
///
/// This axis bounds one source attempting many target addresses. Exhausted
/// buckets return a rate-limit error without a retry interval.
#[implement(super::Service)]
pub fn check_ip_rate_limit(&self, client: IpAddr) -> Result {
	check_bucket(&self.ip_ratelimiter, &client, || client, RC_PER_SECOND, RC_BURST)
}

/// Applies the verification-request limit for one target address.
///
/// This axis bounds many sources attempting the same exact address key.
/// Exhausted buckets return a rate-limit error without a retry interval.
#[implement(super::Service)]
pub fn check_address_rate_limit(&self, address: &str) -> Result {
	check_bucket(
		&self.address_ratelimiter,
		address,
		|| EmailKey::from(address),
		RC_PER_SECOND,
		RC_BURST,
	)
}

fn check_bucket<K, Q>(
	table: &Ratelimiter<K>,
	key: &Q,
	make_key: impl FnOnce() -> K,
	rate: f64,
	burst: f64,
) -> Result
where
	K: Borrow<Q> + Clone + Eq + Hash,
	Q: Eq + Hash + ?Sized,
{
	check_bucket_at(table, key, make_key, rate, burst, Instant::now(), RATELIMIT_MAP_CAP)
}

fn check_bucket_at<K, Q>(
	table: &Ratelimiter<K>,
	key: &Q,
	make_key: impl FnOnce() -> K,
	rate: f64,
	burst: f64,
	now: Instant,
	cap: usize,
) -> Result
where
	K: Borrow<Q> + Clone + Eq + Hash,
	Q: Eq + Hash + ?Sized,
{
	let mut buckets = table.lock()?;
	debug_assert!(cap > 0, "rate-limit table cap must be positive");
	debug_assert!(buckets.len() <= cap, "rate-limit table exceeded its cap");

	if let Some(bucket) = buckets.get_mut(key) {
		return debit_bucket(bucket, rate, burst, now);
	}

	if buckets.len() >= cap {
		let mut oldest = None;

		buckets.retain(|key, bucket| {
			let (last, toks) = *bucket;
			let refilled = now
				.duration_since(last)
				.as_secs_f64()
				.mul_add(rate, toks);

			let retain = refilled < burst;

			if retain
				&& oldest
					.as_ref()
					.is_none_or(|(_, oldest_at)| last < *oldest_at)
			{
				oldest = Some((key.clone(), last));
			}

			retain
		});

		if buckets.len() >= cap
			&& let Some((oldest, _)) = oldest
		{
			buckets.remove::<K>(&oldest);
		}
	}

	let bucket = buckets
		.entry(make_key())
		.or_insert_with(|| (now, burst));

	debit_bucket(bucket, rate, burst, now)
}

fn debit_bucket(bucket: &mut (Instant, f64), rate: f64, burst: f64, now: Instant) -> Result {
	let (last_time, tokens) = bucket;
	let new_tokens = now
		.duration_since(*last_time)
		.as_secs_f64()
		.mul_add(rate, *tokens)
		.min(burst);

	if new_tokens < 1.0 {
		return Err(Error::Request(
			ErrorKind::LimitExceeded(LimitExceededErrorData { retry_after: None }),
			"Too many verification requests.".into(),
			StatusCode::TOO_MANY_REQUESTS,
		));
	}

	*last_time = now;
	*tokens = new_tokens - 1.0;

	Ok(())
}
