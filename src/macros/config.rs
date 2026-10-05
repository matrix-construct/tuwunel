use std::{
	borrow::Cow,
	collections::BTreeMap,
	ffi::c_int,
	fmt::Write as _,
	fs,
	sync::{Mutex, OnceLock, PoisonError},
};

use proc_macro::TokenStream;
use proc_macro2::{Ident, TokenStream as TokenStream2};
use quote::{ToTokens, format_ident, quote};
use syn::{
	Error, Expr, ExprLit, Field, Fields, FieldsNamed, ItemStruct, Lit, LitStr, Meta, MetaList,
	MetaNameValue, Token, Type, TypePath, ext::IdentExt as _, parse::Parser,
	punctuated::Punctuated, spanned::Spanned,
};

use crate::{
	Result,
	utils::{get_simple_settings, is_cargo_compile, is_cargo_test},
};

const UNDOCUMENTED: &str = "# This item is undocumented. Please contribute documentation for it.";

const HIDDEN: &[&str] = &["default", "display", "config-example"];

// Per-filename buffer, accumulated across all macro invocations in this rustc
// process and flushed once at process exit. The `global` section truncates the
// buffer; subsequent sections append. The flush hook compares the accumulated
// buffer against the file on disk and only rewrites when content differs, so
// `cargo check` / `clippy` runs that produce identical output do not bump the
// file mtime.
static FILE_BUFFERS: Mutex<BTreeMap<String, Vec<u8>>> = Mutex::new(BTreeMap::new());
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
		.map_or("", String::as_str)
		.split_ascii_whitespace();

	let contains = |fields: &str, name: &str| {
		fields
			.split_ascii_whitespace()
			.any(|field| field == name)
	};

	let truncate = section == "global";
	let mut section_buf = String::new();

	if let Some(header) = settings.get("header") {
		section_buf.push_str(header);
	}

	let comment_prefix = if section != "global" { "\n#" } else { "" };

	write!(&mut section_buf, "\n\n{comment_prefix}[{section}]\n")
		.expect("written to section buffer");

	let (summary, fields) = if let Fields::Named(FieldsNamed { named, .. }) = &input.fields {
		let capacity = named.len();

		named
			.iter()
			.filter(|field| get_type_name(field).is_some())
			.filter_map(|field| field.ident.as_ref().map(|ident| (field, ident)))
			.fold(
				(Vec::with_capacity(capacity), Vec::with_capacity(capacity)),
				|(mut summary, mut fields), (field, ident)| {
					let name = ident.to_string();
					let is_hidden = contains(hidden, name.as_str());
					let (class, documented) = match () {
						| () if contains(forbidden, name.as_str()) =>
							(quote! { crate::config::regenerate::FieldClass::Forbidden }, false),
						| () if is_hidden =>
							(quote! { crate::config::regenerate::FieldClass::Hidden }, false),
						| () if contains(ignore, name.as_str()) =>
							(quote! { crate::config::regenerate::FieldClass::Structural }, false),
						| () =>
							(quote! { crate::config::regenerate::FieldClass::Documented }, true),
					};

					let example = example_value(field);
					let aliases = get_serde_aliases(field);
					let spec_example = is_hidden
						.then_some(example.as_ref())
						.unwrap_or_default();

					let field_spec = quote! {
						crate::config::regenerate::FieldSpec {
							name: #name,
							aliases: &[#(#aliases),*],
							example: #spec_example,
							class: #class,
						}
					};

					fields.push(field_spec);

					if !documented {
						return (summary, fields);
					}

					let doc = get_doc_comment(field)
						.unwrap_or_else(|| undocumented.into())
						.trim_end()
						.to_owned();

					// A `reloadable:` directive alone does not satisfy the documentation
					// request; prepend the undocumented placeholder when prose is absent.
					let doc = if doc.lines().all(|line| {
						let body = line.trim_start_matches('#').trim();
						body.is_empty() || body.starts_with("reloadable:")
					}) {
						format!("{undocumented}\n{doc}")
					} else {
						doc
					};

					let doc = if doc.ends_with('#') {
						format!("{doc}\n")
					} else {
						format!("{doc}\n#\n")
					};

					let example_separator = (!example.is_empty())
						.then_some(" ")
						.unwrap_or_default();

					write!(&mut section_buf, "\n{doc}").expect("written to section buffer");

					writeln!(&mut section_buf, "#{ident} ={example_separator}{example}")
						.expect("written to section buffer");

					let display = get_doc_comment_line(field, "display");
					let display_directive = |key: &str| {
						display
							.as_ref()
							.into_iter()
							.flat_map(|display| display.split(' '))
							.any(|directive| directive == key)
					};

					if !display_directive("hidden") {
						let value = display_value(ident, display_directive);

						let display = quote! {
							writeln!(out, "| {} | {} |", #name, #value)?;
						};

						summary.push(display);
					}

					(summary, fields)
				},
			)
	} else {
		(Vec::new(), Vec::new())
	};

	if let Some(footer) = settings.get("footer") {
		section_buf.push_str(footer);
	}

	if emit {
		append_section(filename, truncate, section_buf.as_bytes());
	}

	let struct_name = &input.ident;
	let cfg_attrs = cfg_attrs(input)?;
	let registration =
		generate_registration(input, &cfg_attrs, section, section_aliases, &section_buf, &fields);

	let display = quote! {
		#(#cfg_attrs)*
		impl std::fmt::Display for #struct_name {
			fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
				writeln!(out, "| name | value |")?;
				writeln!(out, "| :--- | :---  |")?;
				#( #summary )*
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

fn generate_registration<'a, SectionAliases>(
	input: &ItemStruct,
	cfg_attrs: &[TokenStream2],
	section: &str,
	section_aliases: SectionAliases,
	section_buf: &str,
	fields: &[TokenStream2],
) -> TokenStream2
where
	SectionAliases: Iterator<Item = &'a str>,
{
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
				example: #section_buf,
				fields: &[#(#fields),*],
				position: crate::config::regenerate::SourcePosition {
					file: file!(),
					line: line!(),
					column: column!(),
				},
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

fn append_section(filename: &str, truncate: bool, content: &[u8]) {
	let mut buffers = FILE_BUFFERS
		.lock()
		.unwrap_or_else(PoisonError::into_inner);

	let buf = buffers.entry(filename.to_owned()).or_default();

	if truncate {
		buf.clear();
	}

	buf.extend_from_slice(content);
	drop(buffers);

	FLUSH_REGISTERED.get_or_init(|| {
		atexit(flush_file_buffers);
	});
}

extern "C" fn flush_file_buffers() {
	let buffers = FILE_BUFFERS
		.lock()
		.unwrap_or_else(PoisonError::into_inner);

	for (filename, buf) in buffers.iter() {
		let unchanged =
			fs::read(filename).is_ok_and(|existing| existing.as_slice() == buf.as_slice());

		if !unchanged {
			fs::write(filename, buf).ok();
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

fn get_doc_comment(field: &Field) -> Option<String> {
	let comment = get_doc_comment_full(field)?;

	let out = comment
		.lines()
		.filter(|line| {
			!HIDDEN.iter().any(|key| {
				line.trim().starts_with(key) && line.trim().chars().nth(key.len()) == Some(':')
			})
		})
		.fold(String::new(), |mut full, line| {
			full.push('#');
			full.push_str(line);
			full.push('\n');
			full
		});

	(!out.is_empty()).then_some(out)
}

fn get_doc_comment_line(field: &Field, label: &str) -> Option<String> {
	let comment = get_doc_comment_full(field)?;

	comment
		.lines()
		.map(str::trim)
		.filter(|line| line.starts_with(label))
		.filter(|line| line.chars().nth(label.len()) == Some(':'))
		.map(|line| {
			line.split_once(':')
				.map(|(_, v)| v)
				.map(str::trim)
				.map(ToOwned::to_owned)
		})
		.next()
		.flatten()
}

fn get_doc_comment_full(field: &Field) -> Option<String> {
	let mut out = String::new();
	for attr in &field.attrs {
		let Meta::NameValue(MetaNameValue { path, value, .. }) = &attr.meta else {
			continue;
		};

		if path
			.segments
			.iter()
			.next()
			.is_none_or(|s| s.ident != "doc")
		{
			continue;
		}

		let Expr::Lit(ExprLit { lit, .. }) = &value else {
			continue;
		};

		let Lit::Str(token) = &lit else {
			continue;
		};

		let value = token.value();
		writeln!(&mut out, "{value}").expect("wrote to output string buffer");
	}

	(!out.is_empty()).then_some(out)
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
	use syn::{Field, parse_quote};

	use super::{example_value, get_default, get_doc_comment, get_serde_aliases};

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

		let comment = get_doc_comment(&field).expect("visible comment to be emitted");
		assert!(comment.contains("#visible setting docs"));
		assert!(!comment.contains("config-example:"));
	}
}
