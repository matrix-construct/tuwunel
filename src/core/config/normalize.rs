#[cfg(test)]
mod tests;

use std::{cell::RefCell, iter::once};

use figment::{
	Error, Figment, Metadata, Profile, Provider,
	error::Kind,
	providers::Env,
	util::nest,
	value::{Dict, Map, Tag, Value},
};
use serde::de::IgnoredAny;
use smallstr::SmallString;
use smallvec::SmallVec;

use super::regenerate::{FieldClass, FieldSpec, SectionSpec, schema};
use crate::utils::BoolExt as _;

type EnvironmentKey = SmallString<[u8; 64]>;

type SourceData = Map<Profile, Dict>;

type PathKeys<'a> = SmallVec<[&'a str; 4]>;

type Rewrites = SmallVec<[Rewrite; 4]>;

#[derive(Clone)]
struct Rewrite {
	profile: Profile,
	tag: Tag,
	from: &'static str,
	to: &'static str,
}

struct Normalized {
	data: RefCell<Option<SourceData>>,
	metadata: Metadata,
	profile: Option<Profile>,
	metadata_map: Option<Map<Tag, Metadata>>,
}

pub(super) fn environment(prefix: &'static str) -> Result<impl Provider, Error> {
	let aggregate = Env::raw()
		.filter(move |key| key.starts_with(prefix))
		.lowercase(false)
		.iter()
		.filter_map(|(name, value)| environment_assignment(prefix, name.as_str(), &value))
		.fold(Figment::new(), Figment::merge);

	let provider = Normalized {
		data: RefCell::new(Some(aggregate.data()?)),
		metadata: Env::prefixed(prefix).metadata(),
		metadata_map: aggregate.__metadata_map(),
		profile: None,
	};

	normalize(provider)
}

fn environment_assignment(prefix: &str, name: &str, value: &str) -> Option<Normalized> {
	let path = name
		.get(prefix.len()..)
		.map(str::trim)
		.map(|path| path.replace("__", "."))
		.map(lowercase)
		.filter(|path| path.split('.').any(str::is_empty).is_false())?;

	let value = value
		.parse()
		.expect("environment values parse infallibly");

	let values = nest(&path, value)
		.into_dict()
		.expect("nonempty environment path produces a dictionary");

	let name = name.to_owned();
	let metadata = Metadata::named(format!("environment variable {name}"))
		.interpolater(move |_, _| name.clone());

	Some(Normalized {
		data: RefCell::new(Some(Profile::Global.collect(values))),
		metadata,
		metadata_map: None,
		profile: None,
	})
}

fn lowercase(mut value: String) -> String {
	value.make_ascii_lowercase();
	value
}

pub(super) fn normalize(provider: impl Provider) -> Result<impl Provider, Error> {
	normalize_with(provider, schema())
}

fn normalize_with(
	provider: impl Provider,
	sections: &[&SectionSpec],
) -> Result<Normalized, Error> {
	let data = provider.data()?;
	let rewrites = data
		.iter()
		.flat_map(|(profile, values)| rewrites_for(profile, values, sections))
		.collect::<Rewrites>();

	let metadata = provenance(provider.metadata(), &rewrites, Tag::Default);
	let metadata_map = provider.__metadata_map().map(|metadata| {
		metadata
			.into_iter()
			.map(|(tag, metadata)| (tag, provenance(metadata, &rewrites, tag)))
			.collect()
	});

	let data = data
		.into_iter()
		.map(|(profile, values)| {
			normalize_values(values, aliases_in(sections)).map(|values| (profile, values))
		})
		.collect::<Result<_, _>>()
		.map_err(|error| attributed(error, metadata.clone()))?;

	Ok(Normalized {
		data: RefCell::new(Some(data)),
		metadata,
		metadata_map,
		profile: provider.profile(),
	})
}

fn rewrites_for<'a>(
	profile: &'a Profile,
	values: &'a Dict,
	sections: &'a [&'a SectionSpec],
) -> impl Iterator<Item = Rewrite> + 'a {
	aliases_in(sections).filter_map(move |(from, field)| {
		find(values, from).map(|value| Rewrite {
			profile: profile.clone(),
			tag: value.tag(),
			from,
			to: field.path,
		})
	})
}

impl Provider for Normalized {
	#[inline]
	fn metadata(&self) -> Metadata { self.metadata.clone() }

	fn data(&self) -> Result<Map<Profile, Dict>, Error> {
		self.data
			.borrow_mut()
			.take()
			.ok_or_else(|| Error::from("Normalized configuration source was already consumed."))
	}

	#[inline]
	fn profile(&self) -> Option<Profile> { self.profile.clone() }

	#[inline]
	fn __metadata_map(&self) -> Option<Map<Tag, Metadata>> { self.metadata_map.clone() }
}

fn aliases() -> impl Iterator<Item = (&'static str, &'static FieldSpec)> + Clone {
	aliases_in(schema())
}

fn aliases_in<'a>(
	sections: &'a [&SectionSpec],
) -> impl Iterator<Item = (&'static str, &'a FieldSpec)> + Clone {
	sections
		.iter()
		.flat_map(|section| section.fields)
		.flat_map(|field| {
			field
				.legacy
				.iter()
				.map(move |from| (*from, field))
		})
}

fn provenance(metadata: Metadata, rewrites: &Rewrites, tag: Tag) -> Metadata {
	let rewrites = rewrites
		.iter()
		.filter(|rewrite| rewrite.tag == tag)
		.cloned()
		.collect::<Rewrites>();

	let original = metadata.clone();

	metadata.interpolater(move |profile, keys| {
		let path = keys.join(".");
		let native = rewrites
			.iter()
			.find_map(|rewrite| {
				profile
					.eq(&rewrite.profile)
					.and_then(|| path.strip_prefix(rewrite.to))
					.filter(|suffix| suffix.is_empty() || suffix.starts_with('.'))
					.map(|suffix| format!("{}{suffix}", rewrite.from))
			})
			.unwrap_or(path);

		let prefix = original
			.name
			.strip_prefix('`')
			.and_then(|name| name.split_once('`'));

		match prefix {
			| Some((prefix, _)) => format!("{prefix}{}", environment_key(&native)),
			| None => original.interpolate(profile, &native.split('.').collect::<PathKeys<'_>>()),
		}
	})
}

fn environment_key(native: &str) -> EnvironmentKey {
	native
		.chars()
		.flat_map(|character| {
			let separator = character == '.';
			let character = if separator { '_' } else { character.to_ascii_uppercase() };

			once(character).chain(separator.then_some('_'))
		})
		.collect()
}

fn normalize_values<'a, Aliases>(values: Dict, aliases: Aliases) -> Result<Dict, Error>
where
	Aliases: Iterator<Item = (&'static str, &'a FieldSpec)> + Clone,
{
	aliases.clone().try_for_each(|(from, field)| {
		let duplicate = field
			.legacy
			.iter()
			.copied()
			.chain([field.path])
			.find(|other| *other != from && find(&values, other).is_some());

		duplicate
			.filter(|_| find(&values, from).is_some())
			.map_or(Ok(()), |other| {
				Err(Error::from(format!(
					"Conflicting configuration paths {from:?} and {other:?}."
				)))
			})
	})?;

	rewrite_values(values, aliases)
}

fn rewrite_values<'a, Aliases>(values: Dict, mut aliases: Aliases) -> Result<Dict, Error>
where
	Aliases: Iterator<Item = (&'static str, &'a FieldSpec)>,
{
	aliases.try_fold(values, |mut values, (from, field)| {
		let Some(value) = take(&mut values, from) else {
			return Ok(values);
		};

		insert(&mut values, field.path, value)?;
		Ok(values)
	})
}

fn attributed(mut error: Error, metadata: Metadata) -> Error {
	error.metadata = Some(metadata);
	error
}

pub(super) fn unknown(raw: &Figment) -> impl Iterator<Item = String> + '_ {
	schema()
		.iter()
		.filter(|section| section.group)
		.flat_map(move |section| unknown_in(raw, section))
}

#[expect(clippy::zero_sized_map_values)]
fn unknown_in<'a>(
	raw: &'a Figment,
	section: &'static SectionSpec,
) -> impl Iterator<Item = String> + 'a {
	let path = section.section.trim_start_matches("global.");
	let known = move |key: &str| {
		section
			.fields
			.iter()
			.any(|field| field.name == key)
	};

	raw.extract_inner(path)
		.into_iter()
		.flat_map(move |values: Map<String, IgnoredAny>| {
			values
				.into_keys()
				.filter(move |key| known(key).is_false())
				.map(move |key| format!("{path}.{key}"))
		})
}

pub(super) fn redact_error(error: Error) -> Error {
	error
		.into_iter()
		.map(|error| {
			let moved = aliases().any(|(_, field)| path_overlaps(&error.path, field.path));

			if moved { redact_leaf(error) } else { error }
		})
		.reduce(Error::chain)
		.expect("a configuration error has at least one cause")
}

fn path_overlaps(path: &[String], field: &str) -> bool {
	if path.is_empty() {
		return false;
	}

	path.iter()
		.zip(field.split('.'))
		.all(|(part, expected)| part == expected)
}

fn redact_leaf(mut error: Error) -> Error {
	error.kind = Kind::Message("Invalid configuration value (contents omitted).".to_owned());
	error
}

pub(super) fn assert_canonical(raw: &Figment) -> Result<(), Error> {
	match aliases().find(|(from, _)| raw.contains(from)) {
		| None => Ok(()),
		| Some((from, _)) =>
			Err(format!("Configuration provider was not normalized at {from:?}.").into()),
	}
}

pub(super) fn validate_registry(sections: &[&SectionSpec]) {
	for section in sections {
		let path = section.section.trim_matches(['[', ']']);
		let path = path.strip_prefix("global.").unwrap_or(path);

		assert!(!reserved(path), "reserved canonical configuration section");
		assert!(
			section
				.fields
				.iter()
				.all(|field| !reserved(field.path)),
			"reserved canonical configuration field"
		);
	}

	for (index, (from, field)) in aliases_in(sections).enumerate() {
		assert!(
			aliases_in(sections)
				.take(index)
				.all(|(previous, _)| !overlapping(previous, from)),
			"overlapping legacy configuration path: {from}"
		);
		assert!(field.class != FieldClass::Forbidden, "legacy alias targets a forbidden control");
		assert!(!reserved(from), "reserved legacy configuration path");
		assert!(
			valid_source(sections, from, field),
			"legacy configuration path collides with a canonical field or alias: {from}"
		);
	}
}

fn reserved(path: &str) -> bool { path.split('.').next().eq(&Some("config")) }

fn overlapping(left: &str, right: &str) -> bool {
	left.split('.')
		.zip(right.split('.'))
		.all(|(left, right)| left == right)
}

fn valid_source(sections: &[&SectionSpec], from: &str, target: &FieldSpec) -> bool {
	sections
		.iter()
		.flat_map(|section| section.fields)
		.all(|candidate| {
			let ancestor = from
				.strip_prefix(candidate.path)
				.is_some_and(|tail| tail.starts_with('.'));

			let canonical = !overlapping(candidate.path, from)
				|| (ancestor && registered_group(sections, candidate));

			let aliases = candidate.aliases.iter().all(|alias| {
				target.path == candidate.path || distinct_alias(candidate, alias, from)
			});

			canonical && aliases
		})
}

fn registered_group(sections: &[&SectionSpec], field: &FieldSpec) -> bool {
	field.class == FieldClass::Structural
		&& sections.iter().any(|section| {
			section.group
				&& section
					.section
					.trim_matches(['[', ']'])
					.strip_prefix("global.")
					.eq(&Some(field.path))
		})
}

fn distinct_alias(field: &FieldSpec, alias: &str, from: &str) -> bool {
	field
		.path
		.rsplit_once('.')
		.into_iter()
		.flat_map(|(parent, _)| parent.split('.'))
		.chain(alias.split('.'))
		.zip(from.split('.'))
		.any(|(left, right)| left != right)
}

fn find<'a>(values: &'a Dict, path: &str) -> Option<&'a Value> {
	match path.split_once('.') {
		| None => values.get(path),
		| Some((head, tail)) => find(values.get(head)?.as_dict()?, tail),
	}
}

fn take(values: &mut Dict, path: &str) -> Option<Value> {
	match path.split_once('.') {
		| None => values.remove(path),
		| Some((head, tail)) => {
			let Value::Dict(_, children) = values.get_mut(head)? else {
				return None;
			};

			take(children, tail)
		},
	}
}

fn insert(values: &mut Dict, path: &str, value: Value) -> Result<(), Error> {
	let Some((head, tail)) = path.split_once('.') else {
		values.insert(path.to_owned(), value);
		return Ok(());
	};

	let child = values
		.entry(head.to_owned())
		.or_insert_with(|| Value::Dict(value.tag(), Dict::new()));

	let Value::Dict(_, children) = child else {
		return Err(
			format!("Configuration path {path:?} encounters scalar ancestor {head:?}.").into()
		);
	};

	insert(children, tail, value)
}
