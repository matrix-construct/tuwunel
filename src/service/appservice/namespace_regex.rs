use regex::{RegexSet, RegexSetBuilder};
use ruma::api::appservice::Namespace;
use tuwunel_core::Result;

/// Compiled regular expressions for a namespace
#[derive(Clone, Debug)]
pub struct NamespaceRegex {
	pub exclusive: Option<RegexSet>,
	pub non_exclusive: Option<RegexSet>,
}

impl NamespaceRegex {
	pub(super) fn new<'a, I>(case_sensitive: bool, value: I) -> Result<Self, regex::Error>
	where
		I: Iterator<Item = &'a Namespace> + Clone + Send,
	{
		let exclusive = value
			.clone()
			.filter(|namespace| namespace.exclusive)
			.map(|namespace| namespace.regex.as_str());

		let non_exclusive = value
			.filter(|namespace| !namespace.exclusive)
			.map(|namespace| namespace.regex.as_str());

		Ok(Self {
			exclusive: exclusive
				.clone()
				.count()
				.gt(&0)
				.then(|| {
					RegexSetBuilder::new(exclusive)
						.case_insensitive(!case_sensitive)
						.build()
				})
				.transpose()?,

			non_exclusive: non_exclusive
				.clone()
				.count()
				.gt(&0)
				.then(|| {
					RegexSetBuilder::new(non_exclusive)
						.case_insensitive(!case_sensitive)
						.build()
				})
				.transpose()?,
		})
	}

	/// Checks if this namespace has rights to a namespace
	#[inline]
	#[must_use]
	pub fn is_match(&self, input: &str) -> bool {
		self.is_exclusive_match(input)
			|| self
				.non_exclusive
				.as_ref()
				.is_some_and(|non_exclusive| non_exclusive.is_match(input))
	}

	/// Checks if this namespace has exclusive rights to a namespace
	#[inline]
	#[must_use]
	pub fn is_exclusive_match(&self, input: &str) -> bool {
		self.exclusive
			.as_ref()
			.is_some_and(|exclusive| exclusive.is_match(input))
	}
}
