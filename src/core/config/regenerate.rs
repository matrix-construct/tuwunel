use std::{
	cmp::Ordering,
	fmt::Write as _,
	iter::once,
	path::{Path, PathBuf},
	sync::LazyLock,
};

mod overlay;
#[cfg(test)]
mod tests;
mod tree;
mod write;

use figment::{
	Figment,
	providers::{Data, Format as _},
	value::{Dict, Value},
};
use itertools::Itertools as _;
use link_section::TypedSection;
use serde::Serialize as _;
use smallvec::SmallVec;
use toml::{Value as TomlValue, ser::ValueSerializer};
use toml_writer::TomlWrite as _;

use self::{
	overlay::{filter_nonconfig_environment, is_file_value, validate_file_overlay},
	tree::{
		ConfigPath, Instance, PathPart, find_path, normalize_aliases, remove_path,
		resolve_instances,
	},
	write::write_atomic,
};
use super::{Config, DEPRECATED_KEYS, Sources, format::Toml};
use crate::{Err, Error, Result, err, implement, utils::BoolExt};

const NEVER_EMIT: [&str; 2] = ["database_restore_backup", "force_migration"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FieldClass {
	Documented,
	Structural,
	Hidden,
	Forbidden,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct FieldSpec {
	pub(super) name: &'static str,
	pub(super) aliases: &'static [&'static str],
	pub(super) example: &'static str,
	pub(super) class: FieldClass,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SectionSpec {
	pub(super) section: &'static str,
	pub(super) aliases: &'static [&'static str],
	pub(super) example: &'static str,
	pub(super) fields: &'static [FieldSpec],
	pub(super) position: SourcePosition,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct SourcePosition {
	pub(super) file: &'static str,
	pub(super) line: u32,
	pub(super) column: u32,
}

/// Selects how an existing configuration is regenerated.
///
/// The output path is optional so a single input can use its adjacent `.new`
/// destination. The remaining flags control replacement and value selection.
#[derive(Clone, Copy, Debug, Default)]
pub struct RegenerateOptions<'a> {
	/// Selects the output path.
	///
	/// When omitted, regeneration writes beside its sole input with a `.new`
	/// suffix. Layered inputs require an explicit destination.
	pub output: Option<&'a Path>,

	/// Allows replacement of an existing output.
	///
	/// The previous contents are retained beside the output with a `.bak`
	/// suffix before the atomic replacement is installed.
	pub force: bool,

	/// Materializes configuration values supplied through the environment.
	///
	/// The default keeps file values active and annotates environment overrides
	/// without copying them into the regenerated document.
	pub include_env: bool,

	/// Comments out deprecated and unknown residue keys.
	///
	/// Hidden but valid configuration remains active. Forbidden migration
	/// controls are always removed regardless of this setting.
	pub strip_unknown: bool,
}

/// Selects whether an existing output may be replaced.
///
/// Replacement preserves the previous file beside the output as a `.bak`
/// backup before installing the regenerated contents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Overwrite {
	/// Refuses to replace an existing output.
	///
	/// A preexisting destination is left untouched and causes an error.
	Deny,

	/// Replaces an existing output after retaining a backup.
	///
	/// The previous regular file is preserved at the adjacent backup path.
	Allow,
}

/// Summarizes a completed configuration regeneration.
///
/// Counts distinguish input layers, schema fields, and preserved residue.
/// Intentionally removed migration controls are available through
/// [`Self::dropped_keys`].
#[derive(Debug)]
pub struct RegenerationSummary {
	output: PathBuf,
	input_count: usize,
	configured: usize,
	residue: usize,
	dropped: [bool; NEVER_EMIT.len()],
}

#[derive(Default)]
struct RenderStats {
	configured: usize,
	residue: usize,
	dropped: [bool; NEVER_EMIT.len()],
}

struct RenderContext<'a> {
	environment: Option<(&'a Figment, &'a Dict)>,
	strip_unknown: bool,
	expected: &'a mut Dict,
	stats: &'a mut RenderStats,
}

#[derive(Clone, Copy)]
enum ResidueKind {
	Deprecated,
	Hidden,
	Unknown,
}

#[derive(Clone, Copy)]
enum ResidueDisposition {
	Active,
	Commented,
}

#[link_section::section(typed)]
pub(super) static REGISTERED_SECTIONS: TypedSection<SectionSpec>;

type OrderedSections = SmallVec<[&'static SectionSpec; 16]>;

static ORDERED_SECTIONS: LazyLock<OrderedSections> = LazyLock::new(ordered_sections);

#[inline]
fn schema() -> &'static [&'static SectionSpec] { ORDERED_SECTIONS.as_slice() }

fn ordered_sections() -> OrderedSections {
	let mut sections = REGISTERED_SECTIONS
		.as_slice()
		.iter()
		.collect::<OrderedSections>();

	// Linker-section order is unspecified, so restore source declaration order.
	sections.sort_unstable_by_key(|section| section.position);
	validate_schema(&sections);

	sections
}

fn validate_schema(sections: &[&SectionSpec]) {
	let first = sections
		.first()
		.expect("configuration schema must contain the global section");

	assert!(
		sections
			.iter()
			.all(|section| section.position.file == first.position.file),
		"configuration sections must be declared in one source file",
	);

	assert_eq!(first.section, "global", "global must be the first configuration section");

	for pair in sections.windows(2) {
		assert_ne!(
			pair[0].position, pair[1].position,
			"configuration sections share a source location"
		);
	}

	for (index, section) in sections.iter().enumerate() {
		assert!(
			sections[..index]
				.iter()
				.all(|previous| previous.section != section.section),
			"configuration section {:?} is registered more than once",
			section.section,
		);
	}
}

/// Renders the example configuration carried by this binary.
///
/// The returned document is generated from the same runtime schema that
/// produces the checked-in example file.
pub fn example_config() -> Result<String> {
	let values = Dict::new();
	let mut expected = Dict::new();
	let mut stats = RenderStats::default();
	let rendered = {
		let mut context = RenderContext {
			environment: None,
			strip_unknown: false,
			expected: &mut expected,
			stats: &mut stats,
		};

		render_schema(&values, &mut context)?
	};

	Ok(rendered)
}

/// Writes the example configuration to a filesystem path.
///
/// New files use private permissions. [`Overwrite::Allow`] retains an existing
/// file beside the replacement with a `.bak` suffix.
pub fn write_example_config(path: &Path, overwrite: Overwrite) -> Result {
	let rendered = example_config()?;

	write_atomic(path, rendered.as_bytes(), matches!(overwrite, Overwrite::Allow))
}

/// Regenerates the file-backed configuration retained by `sources`.
///
/// Command-line overrides are never consulted. Environment values are either
/// annotated or explicitly materialized according to `options`.
pub fn regenerate_config(
	sources: &Sources,
	options: RegenerateOptions<'_>,
) -> Result<RegenerationSummary> {
	let paths = sources.file_paths().unique().collect_vec();
	let input_count = paths.len();

	if paths.is_empty() {
		return Err!("Configuration regeneration requires at least one input file.");
	}

	if paths.len() > 1 && options.output.is_none() {
		return Err!(
			"Layered configuration files require an explicit output path because they are \
			 collapsed into one document."
		);
	}

	let output = options
		.output
		.map(Path::to_path_buf)
		.unwrap_or_else(|| adjacent_new_path(&paths[0]));

	let files = Config::load_files(paths.iter().map(PathBuf::as_path))?;
	validate_file_overlay(&files)?;

	let file_values = options
		.include_env
		.is_false()
		.then(|| files.extract::<Dict>())
		.transpose()?;

	let effective = Config::merge_environment(files);
	Config::new(&effective)?;

	let mut values = match file_values {
		| Some(values) => values,
		| None => {
			let mut values = effective.extract::<Dict>()?;

			filter_nonconfig_environment(&effective, &mut values)?;
			values
		},
	};

	normalize_aliases(&mut values, schema());

	let effective_values = options
		.include_env
		.is_false()
		.then(|| effective.extract::<Dict>())
		.transpose()?
		.map(|mut values| {
			normalize_aliases(&mut values, schema());

			values
		});

	let mut expected = values.clone();
	let mut stats = RenderStats::default();
	let rendered = {
		let mut context = RenderContext {
			environment: effective_values
				.as_ref()
				.map(|values| (&effective, values)),
			strip_unknown: options.strip_unknown,
			expected: &mut expected,
			stats: &mut stats,
		};

		render_schema(&values, &mut context)?
	};

	verify_rendered(&rendered, &expected)?;
	write_atomic(&output, rendered.as_bytes(), options.force)?;
	verify_file(&output, &expected).map_err(|error| {
		err!(
			"Configuration was written to {} but post-write verification failed. Inspect the \
			 written file and restore its adjacent `.bak` backup if present before retrying: \
			 {error}",
			output.display(),
		)
	})?;

	let summary = RegenerationSummary {
		output,
		input_count,
		configured: stats.configured,
		residue: stats.residue,
		dropped: stats.dropped,
	};

	Ok(summary)
}

/// Returns the path written by regeneration.
///
/// The path is absolute or relative according to the caller's selected
/// destination.
#[implement(RegenerationSummary)]
#[inline]
#[must_use]
pub fn output(&self) -> &Path { &self.output }

/// Returns the number of unique input files collapsed into the output.
///
/// A value greater than one means the result combines layered configuration
/// sources in their normal precedence order.
#[implement(RegenerationSummary)]
#[inline]
#[must_use]
pub fn input_count(&self) -> usize { self.input_count }

/// Returns the number of configured schema fields written.
///
/// Residue keys are counted separately by [`Self::residue`].
#[implement(RegenerationSummary)]
#[inline]
#[must_use]
pub fn configured(&self) -> usize { self.configured }

/// Returns the number of encountered hidden, deprecated, or unknown keys.
///
/// The count includes commented residue when stripping was requested.
#[implement(RegenerationSummary)]
#[inline]
#[must_use]
pub fn residue(&self) -> usize { self.residue }

/// Iterates over migration controls intentionally removed from the output.
///
/// The iterator is empty when neither control appeared in the selected
/// configuration values.
#[implement(RegenerationSummary)]
pub fn dropped_keys(&self) -> impl Iterator<Item = &'static str> + '_ {
	NEVER_EMIT
		.into_iter()
		.zip(self.dropped)
		.filter_map(|(key, dropped)| dropped.then_some(key))
}

fn render_schema(values: &Dict, context: &mut RenderContext<'_>) -> Result<String> {
	let schema = schema();
	let mut output = String::with_capacity(schema.iter().map(|spec| spec.example.len()).sum());

	for &spec in schema {
		let instances = resolve_instances(spec, values);
		let dynamic = is_dynamic(spec.section);

		for instance in &instances {
			render_instance(&mut output, spec, instance, context)?;
		}

		let rendered_instance = instances.is_empty().is_false();

		let rendered_instance = match context.environment {
			| None => rendered_instance,
			| Some(environment @ (_, environment_values)) => {
				let environment_instances = resolve_instances(spec, environment_values);

				environment_instances
					.iter()
					.filter(|candidate| {
						instances
							.iter()
							.all(|instance| instance.path != candidate.path)
					})
					.try_fold(rendered_instance, |_, candidate| {
						render_environment_instance(&mut output, spec, candidate, environment)?;

						Ok::<_, Error>(true)
					})?
			},
		};

		if dynamic || !rendered_instance {
			output.push_str(spec.example);
		}
	}

	Ok(output)
}

fn render_instance(
	output: &mut String,
	spec: &SectionSpec,
	instance: &Instance<'_>,
	context: &mut RenderContext<'_>,
) -> Result {
	for line in spec.example.split_inclusive('\n') {
		let body = line.strip_suffix('\n').unwrap_or(line);

		if is_template_header(body, spec.section) {
			write_active_header(output, spec.section, &instance.section);
			if line.ends_with('\n') {
				output.push('\n');
			}

			continue;
		}

		let Some(name) = assignment_name(body) else {
			output.push_str(line);
			continue;
		};

		let Some(field) = spec
			.fields
			.iter()
			.find(|field| field.class == FieldClass::Documented && field.name == name)
		else {
			output.push_str(line);
			continue;
		};

		let path = field_path(&instance.path, field.name);
		annotate_environment(
			output,
			context.environment,
			&path,
			instance.values.get(field.name),
		)?;

		match instance.values.get(field.name) {
			| None => output.push_str(line),
			| Some(value) => {
				write_assignment(output, field.name, value)?;
				context.stats.configured = context.stats.configured.saturating_add(1);
			},
		}
	}

	render_unlisted_fields(output, spec, instance, context)
}

fn render_environment_instance(
	output: &mut String,
	spec: &SectionSpec,
	instance: &Instance<'_>,
	environment: (&Figment, &Dict),
) -> Result {
	for line in spec.example.split_inclusive('\n') {
		let body = line.strip_suffix('\n').unwrap_or(line);

		if is_template_header(body, spec.section) {
			output.push('#');
			write_active_header(output, spec.section, &instance.section);
			if line.ends_with('\n') {
				output.push('\n');
			}

			continue;
		}

		if let Some(field) = assignment_name(body).and_then(|name| {
			spec.fields
				.iter()
				.find(|field| field.class == FieldClass::Documented && field.name == name)
		}) {
			let path = field_path(&instance.path, field.name);

			annotate_environment(output, Some(environment), &path, None)?;
		}

		output.push_str(line);
	}

	for field in spec
		.fields
		.iter()
		.filter(|field| field.class == FieldClass::Hidden)
	{
		let path = field_path(&instance.path, field.name);

		render_hidden_environment(output, field, Some(environment), &path)?;
	}

	Ok(())
}

fn render_unlisted_fields(
	output: &mut String,
	spec: &SectionSpec,
	instance: &Instance<'_>,
	context: &mut RenderContext<'_>,
) -> Result {
	for field in spec.fields {
		let Some(value) = instance.values.get(field.name) else {
			if field.class == FieldClass::Hidden {
				let path = field_path(&instance.path, field.name);

				render_hidden_environment(output, field, context.environment, &path)?;
			}

			continue;
		};

		let path = field_path(&instance.path, field.name);

		match field.class {
			| FieldClass::Documented | FieldClass::Structural => {},
			| FieldClass::Hidden => {
				annotate_environment(output, context.environment, &path, Some(value))?;
				render_residue(
					output,
					field.name,
					value,
					ResidueKind::Hidden,
					ResidueDisposition::Active,
				)?;

				context.stats.residue = context.stats.residue.saturating_add(1);
			},
			| FieldClass::Forbidden => {
				remove_path(context.expected, &path);
				mark_dropped(context.stats, field.name);
			},
		}
	}

	for (name, value) in instance.values {
		if spec.fields.iter().any(|field| field.name == name) {
			continue;
		}

		let path = field_path(&instance.path, name);
		let kind = is_deprecated(&path)
			.then_some(ResidueKind::Deprecated)
			.unwrap_or(ResidueKind::Unknown);

		let disposition = if context.strip_unknown {
			ResidueDisposition::Commented
		} else {
			ResidueDisposition::Active
		};

		annotate_environment(output, context.environment, &path, Some(value))?;
		render_residue(output, name, value, kind, disposition)?;
		context.stats.residue = context.stats.residue.saturating_add(1);

		if context.strip_unknown {
			remove_path(context.expected, &path);
		}
	}

	Ok(())
}

fn render_hidden_environment(
	output: &mut String,
	field: &FieldSpec,
	environment: Option<(&Figment, &Dict)>,
	path: &[PathPart<'_>],
) -> Result {
	let Some((figment, value)) = environment_value(environment, path) else {
		return Ok(());
	};

	writeln!(
		output,
		"\n# UNDOCUMENTED: `{}` is valid but omitted from the example configuration.",
		field.name
	)
	.expect("written to configuration buffer");

	output.push_str("# currently set by ");
	write_environment_name(output, figment, value, path);
	output.push_str(".\n");

	match field.example {
		| "" => writeln!(output, "#{} =", field.name),
		| example => writeln!(output, "#{} = {example}", field.name),
	}
	.expect("written to configuration buffer");

	Ok(())
}

fn render_residue(
	output: &mut String,
	name: &str,
	value: &Value,
	kind: ResidueKind,
	disposition: ResidueDisposition,
) -> Result {
	match kind {
		| ResidueKind::Hidden => writeln!(
			output,
			"\n# UNDOCUMENTED: `{name}` is valid but omitted from the example configuration."
		)
		.expect("written to configuration buffer"),
		| ResidueKind::Deprecated => {
			writeln!(
				output,
				"\n# DEPRECATED: `{name}` is no longer used by tuwunel and is ignored."
			)
			.expect("written to configuration buffer");

			writeln!(output, "# Preserved from the previous configuration; it can be deleted.")
				.expect("written to configuration buffer");
		},
		| ResidueKind::Unknown => {
			writeln!(output, "\n# UNKNOWN: `{name}` is not a tuwunel configuration option.")
				.expect("written to configuration buffer");

			writeln!(output, "# Preserved from the previous configuration.")
				.expect("written to configuration buffer");
		},
	}

	if matches!(disposition, ResidueDisposition::Commented) {
		output.push('#');
	}

	write_assignment(output, name, value)?;

	Ok(())
}

fn annotate_environment(
	output: &mut String,
	environment: Option<(&Figment, &Dict)>,
	path: &[PathPart<'_>],
	file_value: Option<&Value>,
) -> Result {
	let Some((figment, value)) = environment_value(environment, path) else {
		return Ok(());
	};

	let action = file_value
		.is_some()
		.then_some("currently overridden by")
		.unwrap_or("currently set by");

	write!(output, "# {action} ").expect("written to configuration buffer");
	write_environment_name(output, figment, value, path);
	output.push_str(".\n");

	Ok(())
}

fn environment_value<'a>(
	environment: Option<(&'a Figment, &'a Dict)>,
	path: &[PathPart<'_>],
) -> Option<(&'a Figment, &'a Value)> {
	let (figment, values) = environment?;

	find_path(values, path)
		.filter(|value| !is_file_value(figment, value))
		.map(|value| (figment, value))
}

fn write_assignment(output: &mut String, name: &str, value: &Value) -> Result {
	output
		.key(name)
		.expect("written to configuration buffer");

	output.push_str(" = ");
	write_value(output, value)?;
	output.push('\n');

	Ok(())
}

fn write_value(output: &mut String, value: &Value) -> Result {
	value
		.serialize(ValueSerializer::new(output))
		.map_err(|error| err!("Failed to serialize a configuration value: {error}"))?;

	Ok(())
}

fn assignment_name(line: &str) -> Option<&str> {
	let (name, _) = line.strip_prefix('#')?.split_once(" =")?;

	name.chars()
		.all(|character| {
			character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
		})
		.then_some(name)
}

fn is_template_header(line: &str, section: &str) -> bool {
	match section == "global" {
		| true => line == "[global]",
		| false => line
			.strip_prefix("#[")
			.and_then(|line| line.strip_suffix(']'))
			.is_some_and(|line| line == section),
	}
}

fn write_active_header(output: &mut String, section: &str, resolved: &str) {
	if section.starts_with('[') {
		write!(output, "[[{resolved}]]").expect("written to configuration buffer");
	} else {
		write!(output, "[{resolved}]").expect("written to configuration buffer");
	}
}

fn is_dynamic(section: &str) -> bool { section.contains('<') || section.starts_with('[') }

fn field_path<'a>(base: &ConfigPath<'a>, field: &'a str) -> ConfigPath<'a> {
	base.iter()
		.copied()
		.chain(once(PathPart::Key(field)))
		.collect()
}

fn is_deprecated(path: &[PathPart<'_>]) -> bool {
	DEPRECATED_KEYS.iter().any(|deprecated| {
		deprecated
			.split('.')
			.eq(path.iter().filter_map(|part| match part {
				| PathPart::Key(key) => Some(*key),
				| PathPart::Index(_) => None,
			}))
	})
}

fn mark_dropped(stats: &mut RenderStats, name: &str) {
	if let Some(index) = NEVER_EMIT.iter().position(|key| *key == name) {
		stats.dropped[index] = true;
	}
}

fn write_environment_name(
	output: &mut String,
	figment: &Figment,
	value: &Value,
	path: &[PathPart<'_>],
) {
	let prefix = figment
		.get_metadata(value.tag())
		.and_then(|metadata| metadata.name.strip_prefix('`'))
		.and_then(|name| name.split_once('`'))
		.map(|(prefix, _)| prefix)
		.unwrap_or("TUWUNEL_");

	output.push_str(prefix);

	for (index, key) in path
		.iter()
		.take_while(|part| matches!(part, PathPart::Key(_)))
		.filter_map(|part| match part {
			| PathPart::Key(key) => Some(*key),
			| PathPart::Index(_) => None,
		})
		.enumerate()
	{
		if index > 0 {
			output.push_str("__");
		}

		output.extend(
			key.chars()
				.map(|character| character.to_ascii_uppercase()),
		);
	}
}

fn verify_rendered(rendered: &str, expected: &Dict) -> Result {
	let output = Figment::new().merge(Data::nested(Toml::string(rendered)));
	let actual = output.extract::<Dict>()?;

	if !toml_equivalent(&actual, expected)? {
		return Err!("Regenerated configuration failed its semantic round-trip check.");
	}

	Ok(())
}

fn verify_file(path: &Path, expected: &Dict) -> Result {
	let output = Config::load_files([path].into_iter())?;
	let actual = output.extract::<Dict>()?;

	if !toml_equivalent(&actual, expected)? {
		return Err!("Written configuration failed its semantic round-trip check.");
	}

	Ok(())
}

fn toml_equivalent(left: &Dict, right: &Dict) -> Result<bool> {
	let left = TomlValue::try_from(left)
		.map_err(|error| err!("Failed to normalize regenerated values: {error}"))?;

	let right = TomlValue::try_from(right)
		.map_err(|error| err!("Failed to normalize expected values: {error}"))?;

	Ok(toml_values_equivalent(&left, &right))
}

fn toml_values_equivalent(left: &TomlValue, right: &TomlValue) -> bool {
	match (left, right) {
		| (TomlValue::Float(left), TomlValue::Float(right)) =>
			left.is_nan() && right.is_nan()
				|| left
					.partial_cmp(right)
					.is_some_and(Ordering::is_eq),
		| (TomlValue::Array(left), TomlValue::Array(right)) =>
			left.len() == right.len()
				&& left
					.iter()
					.zip(right)
					.all(|(left, right)| toml_values_equivalent(left, right)),
		| (TomlValue::Table(left), TomlValue::Table(right)) =>
			left.len() == right.len()
				&& left.iter().all(|(key, left)| {
					right
						.get(key)
						.is_some_and(|right| toml_values_equivalent(left, right))
				}),
		| _ => left == right,
	}
}

fn adjacent_new_path(input: &Path) -> PathBuf {
	let mut output = input.as_os_str().to_owned();

	output.push(".new");

	output.into()
}
