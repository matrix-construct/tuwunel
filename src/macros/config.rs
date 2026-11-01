use std::{
	borrow::Cow,
	collections::BTreeMap,
	ffi::c_int,
	fs::{read, write},
	sync::{Mutex, OnceLock, PoisonError},
};

use proc_macro::TokenStream;
use proc_macro2::{Ident, TokenStream as TokenStream2};
use quote::{ToTokens, format_ident, quote};
use syn::{
	Attribute, Error, Expr, ExprLit, Field, Fields, FieldsNamed, ItemStruct, Lit, LitStr, Meta,
	MetaList, MetaNameValue, Token, Type, TypePath, ext::IdentExt as _, parse::Parser,
	punctuated::Punctuated, spanned::Spanned,
};

use crate::{
	Result,
	utils::{get_simple_settings, is_cargo_compile, is_cargo_test},
};

const UNDOCUMENTED: &str = "# This item is undocumented. Please contribute documentation for it.";

const HIDDEN: &[&str] = &["default", "display", "config-example", "legacy"];

const RULE_WIDTH: usize = 80;

type Sections = BTreeMap<String, Vec<u8>>;

struct Registration<'a> {
	input: &'a ItemStruct,
	cfg_attrs: &'a [TokenStream2],
	section: &'a str,
	section_aliases: &'a str,
	about: &'a str,
	section_buf: &'a str,
	fields: &'a [TokenStream2],
	group: bool,
}

struct Rendering<'a> {
	section: &'a str,
	undocumented: &'a str,
	ignore: &'a str,
	hidden: &'a str,
	forbidden: &'a str,
	group: bool,
}

#[derive(Default)]
struct Rendered {
	summary: TokenStream2,
	fields: Vec<TokenStream2>,
	example: String,
}

struct RenderedField {
	summary: TokenStream2,
	spec: TokenStream2,
	example: String,
}

// Section paths give both writers the same parent-before-child ordering.
static FILE_BUFFERS: Mutex<BTreeMap<String, Sections>> = Mutex::new(BTreeMap::new());
static FLUSH_REGISTERED: OnceLock<()> = OnceLock::new();

unsafe extern "C" {
	safe fn atexit(cb: extern "C" fn()) -> c_int;
}

#[expect(clippy::needless_pass_by_value)]
pub(super) fn example_generator(input: ItemStruct, args: &[Meta]) -> Result<TokenStream> {
	let emit = is_cargo_compile() && !is_cargo_test();
	let additional = generate_example(&input, args, emit)?;

	Ok([input.to_token_stream(), additional]
		.into_iter()
		.collect::<TokenStream2>()
		.into())
}

fn generate_example(input: &ItemStruct, args: &[Meta], emit: bool) -> Result<TokenStream2> {
	let settings = get_simple_settings(args);

	let section = settings.get("section").ok_or_else(|| {
		Error::new(args[0].span(), "missing required 'section' attribute argument")
	})?;

	let filename = settings.get("filename").ok_or_else(|| {
		Error::new(args[0].span(), "missing required 'filename' attribute argument")
	})?;

	let undocumented = settings
		.get("undocumented")
		.map_or(UNDOCUMENTED, String::as_str);

	let ignore = settings.get("ignore").map_or("", String::as_str);
	let hidden = settings.get("hidden").map_or("", String::as_str);
	let forbidden = settings
		.get("forbidden")
		.map_or("", String::as_str);

	let section_aliases = settings
		.get("section_aliases")
		.map_or("", String::as_str);

	let group = settings
		.get("group")
		.is_some_and(|value| value == "true");

	let rendering = Rendering {
		section,
		undocumented,
		ignore,
		hidden,
		forbidden,
		group,
	};

	let rendered = if let Fields::Named(FieldsNamed { named, .. }) = &input.fields {
		let capacity = named.len();
		let output = Rendered {
			summary: TokenStream2::new(),
			fields: Vec::with_capacity(capacity),
			example: String::new(),
		};

		named
			.iter()
			.filter(|field| get_type_name(field).is_some())
			.filter_map(|field| field.ident.as_ref().map(|ident| (field, ident)))
			.map(|field| render_field(field, &rendering))
			.fold(output, merge_rendered)
	} else {
		Rendered::default()
	};

	let header = settings.get("header").map_or("", String::as_str);
	let footer = settings.get("footer").map_or("", String::as_str);
	let about = section_documentation(input, header)?;
	let comment_prefix = if section != "global" { "#" } else { "" };
	let Rendered { summary, fields, example } = rendered;
	let section_buf = format!("{comment_prefix}[{section}]\n{example}{footer}");

	if emit {
		append_section(filename, section, &[about.as_bytes(), section_buf.as_bytes()]);
	}

	let struct_name = &input.ident;
	let cfg_attrs = cfg_attrs(input)?;
	let registration = Registration {
		input,
		cfg_attrs: &cfg_attrs,
		section,
		section_aliases,
		about: &about,
		section_buf: &section_buf,
		fields: &fields,
		group,
	};

	let registration = generate_registration(&registration);

	let display = quote! {
		#(#cfg_attrs)*
		impl std::fmt::Display for #struct_name {
			fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
				writeln!(out, "| name | value |")?;
				writeln!(out, "| :--- | :---  |")?;
				self.fmt_config(out)
			}
		}

		#(#cfg_attrs)*
		impl #struct_name {
			pub(super) fn fmt_config(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
				#summary
				Ok(())
			}
		}
	};

	let generated = quote! {
		#registration
		#display
	};

	Ok(generated)
}

fn section_documentation(input: &ItemStruct, header: &str) -> Result<String> {
	let doc = get_doc_comment(&input.attrs)
		.ok_or_else(|| Error::new(input.ident.span(), "config section requires a doc comment"))?;

	let (doc, suffix) = trim_comment(&doc);

	Ok(format!("{header}\n\n{:#<RULE_WIDTH$}\n{doc}{suffix}", ""))
}

fn render_field((field, ident): (&Field, &Ident), rendering: &Rendering<'_>) -> RenderedField {
	let Rendering {
		section,
		undocumented,
		ignore,
		hidden,
		forbidden,
		group,
	} = rendering;

	let contains = |fields: &str, name: &str| {
		fields
			.split_ascii_whitespace()
			.any(|field| field == name)
	};

	let name = ident.to_string();
	let is_hidden = contains(hidden, name.as_str());
	let (class, documented) = match () {
		| () if contains(forbidden, name.as_str()) =>
			(quote! { crate::config::regenerate::FieldClass::Forbidden }, false),
		| () if is_hidden => (quote! { crate::config::regenerate::FieldClass::Hidden }, false),
		| () if contains(ignore, name.as_str()) =>
			(quote! { crate::config::regenerate::FieldClass::Structural }, false),
		| () => (quote! { crate::config::regenerate::FieldClass::Documented }, true),
	};

	let example = example_value(field);
	let spec_example = is_hidden
		.then_some(example.as_ref())
		.unwrap_or_default();

	let (path, field_spec) = generate_field_spec(field, section, &name, &class, spec_example);

	let nested = has_display_directive(field, "section")
		.then(|| quote! { self.#ident.fmt_config(out)?; })
		.unwrap_or_default();

	if !documented {
		return RenderedField {
			summary: nested,
			spec: field_spec,
			example: String::new(),
		};
	}

	let doc = field_documentation(field, undocumented);

	let example_separator = if example.is_empty() { "" } else { " " };
	let example = format!("\n{doc}#{ident} ={example_separator}{example}\n");

	let display = has_display_directive(field, "hidden")
		.then_some(TokenStream2::new())
		.unwrap_or_else(|| {
			let value = display_value(ident, |directive| has_display_directive(field, directive));
			let name = if *group { &path } else { &name };

			quote! { writeln!(out, "| {} | {} |", #name, #value)?; }
		});

	RenderedField {
		summary: quote! { #nested #display },
		spec: field_spec,
		example,
	}
}

fn has_display_directive(field: &Field, directive: &str) -> bool {
	get_doc_comment_line(field, "display")
		.unwrap_or_default()
		.split(' ')
		.any(|candidate| candidate == directive)
}

fn merge_rendered(mut output: Rendered, field: RenderedField) -> Rendered {
	output.summary.extend(field.summary);
	output.fields.push(field.spec);
	output.example.push_str(&field.example);
	output
}

fn generate_field_spec(
	field: &Field,
	section: &str,
	name: &str,
	class: &TokenStream2,
	example: &str,
) -> (String, TokenStream2) {
	let aliases = get_serde_aliases(field).collect::<Vec<_>>();
	let section = section.trim_matches(['[', ']']);
	let path = section
		.strip_prefix("global.")
		.map(|section| format!("{section}.{name}"))
		.unwrap_or_else(|| name.to_owned());

	let legacy = get_doc_comment_line(field, "legacy").unwrap_or_default();
	let local_alias = |alias: &LitStr| {
		Cow::Owned(format!("{}.{}", section.trim_start_matches("global."), alias.value()))
	};

	let legacy = legacy
		.split_ascii_whitespace()
		.map(Cow::Borrowed)
		.chain(
			aliases
				.iter()
				.filter(|_| !legacy.is_empty())
				.map(local_alias),
		);

	let sensitive = has_display_directive(field, "sensitive");
	let spec = quote! {
		crate::config::regenerate::FieldSpec {
			name: #name,
			path: #path,
			legacy: &[#(#legacy),*],
			aliases: &[#(#aliases),*],
			example: #example,
			class: #class,
			sensitive: #sensitive,
		}
	};

	(path, spec)
}

fn field_documentation(field: &Field, undocumented: &str) -> String {
	let doc = get_doc_comment(&field.attrs);
	let (doc, suffix) = trim_comment(doc.as_deref().unwrap_or(undocumented));
	let empty = doc.lines().all(|line| {
		let body = line.trim_start_matches('#').trim();

		body.is_empty() || body.starts_with("reloadable:")
	});

	let prefix = if empty { undocumented } else { "" };
	let separator = if empty { "\n" } else { "" };

	format!("{prefix}{separator}{doc}{suffix}")
}

fn trim_comment(doc: &str) -> (&str, &'static str) {
	let doc = doc.trim_end();
	let suffix = if doc.ends_with('#') { "\n" } else { "\n#\n" };

	(doc, suffix)
}

fn display_value(ident: &Ident, directive: impl Fn(&str) -> bool) -> TokenStream2 {
	match () {
		| () if directive("sensitive") => quote! { "***********" },
		| () if directive("uri-userinfo") =>
			quote! { crate::config::format::UriUserinfo(self.#ident.as_deref()) },
		| () => quote! { format_args!("{:?}", self.#ident) },
	}
}

fn cfg_attrs(input: &ItemStruct) -> Result<Vec<TokenStream2>> {
	input
		.attrs
		.iter()
		.try_fold(Vec::new(), |mut cfg_attrs, attribute| {
			if attribute.path().is_ident("cfg") {
				cfg_attrs.push(attribute.to_token_stream());

				return Ok(cfg_attrs);
			}

			let Meta::List(MetaList { path, tokens, .. }) = &attribute.meta else {
				return Ok(cfg_attrs);
			};

			if !path.is_ident("cfg_attr") {
				return Ok(cfg_attrs);
			}

			let mut args = Punctuated::<Meta, Token![,]>::parse_terminated
				.parse2(tokens.clone())?
				.into_iter();

			let Some(predicate) = args.next() else {
				return Err(Error::new(attribute.span(), "cfg_attr requires a predicate"));
			};

			let mut nested = args
				.filter(|meta| meta.path().is_ident("cfg"))
				.peekable();

			if nested.peek().is_some() {
				cfg_attrs.push(quote! { #[cfg_attr(#predicate, #(#nested),*)] });
			}

			Ok(cfg_attrs)
		})
}

fn generate_registration(registration: &Registration<'_>) -> TokenStream2 {
	let Registration {
		input,
		cfg_attrs,
		section,
		section_aliases,
		about,
		section_buf,
		fields,
		group,
	} = registration;

	let section_aliases = section_aliases.split_ascii_whitespace();

	let registration_name = format_ident!(
		"TUWUNEL_CONFIG_SECTION_{}",
		input.ident.unraw().to_string().to_uppercase(),
	);

	quote! {
		#(#cfg_attrs)*
		#[::link_section::in_section(crate::config::regenerate::REGISTERED_SECTIONS)]
		const #registration_name: crate::config::regenerate::SectionSpec =
			crate::config::regenerate::SectionSpec {
				section: #section,
				aliases: &[#(#section_aliases),*],
				about: #about,
				example: #section_buf,
				fields: &[#(#fields),*],
				group: #group,
			};
	}
}

fn get_serde_aliases(field: &Field) -> impl Iterator<Item = LitStr> + '_ {
	field
		.attrs
		.iter()
		.filter(|attr| attr.path().is_ident("serde"))
		.filter_map(|attr| {
			let Meta::List(MetaList { tokens, .. }) = &attr.meta else {
				return None;
			};

			Punctuated::<Meta, Token![,]>::parse_terminated
				.parse2(tokens.clone())
				.ok()
		})
		.flatten()
		.filter_map(|arg| {
			let Meta::NameValue(MetaNameValue {
				path,
				value: Expr::Lit(ExprLit { lit: Lit::Str(alias), .. }),
				..
			}) = arg
			else {
				return None;
			};

			path.is_ident("alias").then_some(alias)
		})
}

fn append_section(filename: &str, section: &str, parts: &[&[u8]]) {
	let mut buffers = FILE_BUFFERS
		.lock()
		.unwrap_or_else(PoisonError::into_inner);

	buffers
		.entry(filename.to_owned())
		.or_default()
		.insert(section.trim_matches(['[', ']']).to_owned(), parts.concat());

	drop(buffers);

	FLUSH_REGISTERED.get_or_init(|| {
		atexit(flush_file_buffers);
	});
}

extern "C" fn flush_file_buffers() {
	let buffers = FILE_BUFFERS
		.lock()
		.unwrap_or_else(PoisonError::into_inner);

	for (filename, sections) in buffers.iter() {
		let buf = sections
			.values()
			.flatten()
			.copied()
			.collect::<Vec<_>>();

		let unchanged =
			read(filename).is_ok_and(|existing| existing.as_slice().eq(buf.as_slice()));

		if !unchanged {
			write(filename, &buf).ok();
		}
	}
}

fn get_default(field: &Field) -> Option<Cow<'static, str>> {
	for attr in &field.attrs {
		let Meta::List(MetaList { path, tokens, .. }) = &attr.meta else {
			continue;
		};

		if path
			.segments
			.iter()
			.next()
			.is_none_or(|s| s.ident != "serde")
		{
			continue;
		}

		let Some(arg) = Punctuated::<Meta, Token![,]>::parse_terminated
			.parse2(tokens.clone())
			.ok()?
			.into_iter()
			.next()
		else {
			continue;
		};

		match arg {
			| Meta::Path { .. } => return Some(Cow::Borrowed("false")),
			| Meta::NameValue(MetaNameValue {
				value: Expr::Lit(ExprLit { lit: Lit::Str(str), .. }),
				..
			}) => {
				match str.value().as_str() {
					| "true_fn" => return Some(Cow::Borrowed("true")),
					| "HashSet::new" | "Vec::new" | "RegexSet::empty" => {
						return Some(Cow::Borrowed("[]"));
					},
					| _ => return None,
				};
			},
			| _ => return None,
		}
	}

	None
}

fn example_value(field: &Field) -> Cow<'static, str> {
	get_doc_comment_line(field, "config-example")
		.map(Cow::Owned)
		.or_else(|| get_doc_comment_line(field, "default").map(Cow::Owned))
		.or_else(|| get_default(field))
		.unwrap_or_default()
}

fn get_doc_comment(attrs: &[Attribute]) -> Option<String> {
	let comment = doc_values(attrs).fold(String::new(), |comment, value| {
		value
			.split('\n')
			.filter(|line| !is_hidden_line(line))
			.fold(comment, push_comment_line)
	});

	Some(comment).filter(|comment| !comment.is_empty())
}

fn get_doc_comment_line(field: &Field, label: &str) -> Option<String> {
	doc_values(&field.attrs).find_map(|value| {
		value
			.split('\n')
			.map(str::trim)
			.find_map(|line| directive(line, label))
			.map(str::trim)
			.map(ToOwned::to_owned)
	})
}

fn doc_values(attrs: &[Attribute]) -> impl Iterator<Item = String> + '_ {
	attrs.iter().filter_map(doc_value)
}

fn doc_value(attr: &Attribute) -> Option<String> {
	let Meta::NameValue(MetaNameValue {
		path,
		value: Expr::Lit(ExprLit { lit: Lit::Str(token), .. }),
		..
	}) = &attr.meta
	else {
		return None;
	};

	path.is_ident("doc").then(|| token.value())
}

fn is_hidden_line(line: &str) -> bool {
	let line = line.trim();

	// Fences keep TOML examples out of rustdoc's doctests.
	line.starts_with("```")
		|| HIDDEN
			.iter()
			.any(|key| directive(line, key).is_some())
}

fn directive<'a>(line: &'a str, key: &str) -> Option<&'a str> {
	line.strip_prefix(key)?.strip_prefix(':')
}

fn push_comment_line(mut comment: String, line: &str) -> String {
	comment.push('#');
	comment.push_str(line);
	comment.push('\n');
	comment
}

fn get_type_name(field: &Field) -> Option<String> {
	let Type::Path(TypePath { path, .. }) = &field.ty else {
		return None;
	};

	path.segments
		.iter()
		.next()
		.map(|segment| segment.ident.to_string())
}

#[cfg(test)]
mod tests {
	use quote::quote;
	use syn::{Field, ItemStruct, parse_quote};

	use super::{
		example_value, generate_field_spec, get_default, get_doc_comment, get_serde_aliases,
		section_documentation,
	};

	#[test]
	fn bracketed_sections_keep_qualified_field_metadata() {
		let field: Field = parse_quote! { client_id: String };
		let class = quote! { FieldClass::Documented };

		for section in [
			"global.identity_provider",
			"[global.identity_provider]",
			"[[global.identity_provider]]",
		] {
			let (path, _) = generate_field_spec(&field, section, "client_id", &class, "");

			assert_eq!(path, "identity_provider.client_id");
		}
	}

	#[test]
	fn empty_collection_defaults_render_as_arrays() {
		let hash_set: Field = parse_quote! {
			#[serde(default = "HashSet::new")]
			value: ()
		};

		let vec: Field = parse_quote! {
			#[serde(default = "Vec::new")]
			value: ()
		};

		let regex_set: Field = parse_quote! {
			#[serde(default = "RegexSet::empty")]
			value: ()
		};

		let fields: [(&str, Field); 3] =
			[("HashSet::new", hash_set), ("Vec::new", vec), ("RegexSet::empty", regex_set)];

		for (name, field) in &fields {
			assert_eq!(get_default(field).as_deref(), Some("[]"), "{name}");
		}
	}

	#[test]
	fn serde_aliases_are_collected_in_declaration_order() {
		let field: Field = parse_quote! {
			#[serde(default, alias = "old_name", alias = "older_name")]
			value: String
		};

		assert!(
			get_serde_aliases(&field)
				.map(|alias| alias.value())
				.eq(["old_name", "older_name"])
		);
	}

	#[test]
	fn config_example_overrides_default_for_generated_example_value() {
		let field: Field = parse_quote! {
			#[doc = "config-example: from example"]
			#[doc = "default: from default"]
			name: String
		};

		assert_eq!(example_value(&field), "from example");
	}

	#[test]
	fn config_example_is_hidden_from_emitted_comments() {
		let field: Field = parse_quote! {
			#[doc = "visible setting docs"]
			#[doc = "config-example: hidden value"]
			name: String
		};

		let comment = get_doc_comment(&field.attrs).expect("visible comment to be emitted");
		assert!(comment.contains("#visible setting docs"));
		assert!(!comment.contains("config-example:"));
	}

	#[test]
	fn section_documentation_renders_rule_doc_and_closing_line() {
		let input: ItemStruct = parse_quote! {
			/// Section title.
			///
			/// Section overview.
			struct Section;
		};

		let about = section_documentation(&input, "").expect("documented section");
		let rule = "#".repeat(80);

		assert_eq!(about, format!("\n\n{rule}\n# Section title.\n#\n# Section overview.\n#\n"));
	}

	#[test]
	fn code_fences_are_hidden_from_emitted_comments() {
		let input: ItemStruct = parse_quote! {
			/// Section title.
			///
			/// ```toml
			///     [section]
			/// ```
			struct Section;
		};

		let about = section_documentation(&input, "").expect("documented section");
		let rule = "#".repeat(80);

		assert_eq!(about, format!("\n\n{rule}\n# Section title.\n#\n#     [section]\n#\n"));
	}

	#[test]
	fn undocumented_section_is_rejected() {
		let input: ItemStruct = parse_quote! { struct Section; };
		let error = section_documentation(&input, "").unwrap_err();

		assert!(error.to_string().contains("doc comment"));
	}
}
