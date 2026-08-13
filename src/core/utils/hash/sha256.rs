use aws_lc_rs::{
	digest,
	digest::{Context, SHA256, SHA256_OUTPUT_LEN},
};

/// Fixed-size output of a SHA-256 digest operation.
///
/// The array contains the algorithm's 32 output bytes in digest order. It can
/// be encoded or compared without an additional allocation.
pub type Digest = [u8; SHA256_OUTPUT_LEN];

/// Sha256 hash (input gather joined by 0xFF bytes)
#[must_use]
#[tracing::instrument(skip(inputs), level = "trace")]
pub fn delimited<'a, T, I>(mut inputs: I) -> Digest
where
	I: Iterator<Item = T> + 'a,
	T: AsRef<[u8]> + 'a,
{
	let mut ctx = Context::new(&SHA256);
	if let Some(input) = inputs.next() {
		ctx.update(input.as_ref());
		for input in inputs {
			ctx.update(b"\xFF");
			ctx.update(input.as_ref());
		}
	}

	ctx.finish()
		.as_ref()
		.try_into()
		.expect("failed to return Digest buffer")
}

/// Sha256 hash (input gather)
#[must_use]
#[tracing::instrument(skip(inputs), level = "trace")]
pub fn concat<'a, T, I>(inputs: I) -> Digest
where
	I: Iterator<Item = T> + 'a,
	T: AsRef<[u8]> + 'a,
{
	inputs
		.fold(Context::new(&SHA256), |mut ctx, input| {
			ctx.update(input.as_ref());
			ctx
		})
		.finish()
		.as_ref()
		.try_into()
		.expect("failed to return Digest buffer")
}

/// Sha256 hash
#[inline]
#[must_use]
#[tracing::instrument(skip(input), level = "trace")]
pub fn hash<T>(input: T) -> Digest
where
	T: AsRef<[u8]>,
{
	digest::digest(&SHA256, input.as_ref())
		.as_ref()
		.try_into()
		.expect("failed to return Digest buffer")
}
