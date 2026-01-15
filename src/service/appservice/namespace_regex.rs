use regex::RegexSet;
use ruma::api::appservice::Namespace;
use tuwunel_core::Result;

/// Compiled regular expressions for a namespace
#[derive(Clone, Debug)]
pub struct NamespaceRegex {
	pub exclusive: Option<RegexSet>,
	pub non_exclusive: Option<RegexSet>,
}

impl NamespaceRegex {
	/// Checks if this namespace has rights to a namespace
	#[inline]
	#[must_use]
	pub fn is_match(&self, heystack: &str) -> bool {
		if self.is_exclusive_match(heystack) {
			return true;
		}

		if let Some(non_exclusive) = &self.non_exclusive
			&& non_exclusive.is_match(heystack)
		{
			return true;
		}
		false
	}

	/// Checks if this namespace has exclusive rights to a namespace
	#[inline]
	#[must_use]
	pub fn is_exclusive_match(&self, heystack: &str) -> bool {
		if let Some(exclusive) = &self.exclusive
			&& exclusive.is_match(heystack)
		{
			return true;
		}
		false
	}
}

impl TryFrom<Vec<Namespace>> for NamespaceRegex {
	type Error = regex::Error;

	fn try_from(value: Vec<Namespace>) -> Result<Self, regex::Error> {
		let mut exclusive = Vec::with_capacity(value.len());
		let mut non_exclusive = Vec::with_capacity(value.len());

		for namespace in value {
			if namespace.exclusive {
				exclusive.push(namespace.regex);
			} else {
				non_exclusive.push(namespace.regex);
			}
		}

		Ok(Self {
			exclusive: if exclusive.is_empty() {
				None
			} else {
				Some(RegexSet::new(exclusive)?)
			},
			non_exclusive: if non_exclusive.is_empty() {
				None
			} else {
				Some(RegexSet::new(non_exclusive)?)
			},
		})
	}
}
