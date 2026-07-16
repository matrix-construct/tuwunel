use std::{
	collections::{BTreeMap, HashSet},
	ffi::c_int,
	fmt::Write as _,
	fs,
	sync::{Mutex, OnceLock, PoisonError},
};

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{ToTokens, quote};
use syn::{
	Error, Expr, ExprLit, Field, Fields, FieldsNamed, ItemStruct, Lit, Meta, MetaList,
	MetaNameValue, Type, TypePath, parse::Parser, punctuated::Punctuated, spanned::Spanned,
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

#[allow(clippy::needless_pass_by_value)]
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

	let ignore: HashSet<&str> = settings
		.get("ignore")
		.map_or("", String::as_str)
		.split(' ')
		.collect();

	let truncate = section == "global";
	let mut section_buf = String::new();

	if emit {
		if let Some(header) = settings.get("header") {
			section_buf.push_str(header);
		}

		let pound = if section != "global" { "\n#" } else { "" };
		write!(&mut section_buf, "\n\n{pound}[{section}]\n").expect("written to section buffer");
	}

	let mut summary: Vec<TokenStream2> = Vec::new();
	if let Fields::Named(FieldsNamed { named, .. }) = &input.fields {
		for field in named {
			let Some(ident) = &field.ident else {
				continue;
			};

			if ignore.contains(ident.to_string().as_str()) {
				continue;
			}

			if get_type_name(field).is_none() {
				continue;
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

			// `config-example` overrides the emitted example value while `default`
			// continues to document the runtime default separately.
			let default = example_value(field);

			let default = if !default.is_empty() {
				format!(" {default}")
			} else {
				default
			};

			if emit {
				write!(&mut section_buf, "\n{doc}").expect("written to section buffer");

				writeln!(&mut section_buf, "#{ident} ={default}")
					.expect("written to section buffer");
			}

			let display = get_doc_comment_line(field, "display");
			let display_directive = |key| {
				display
					.as_ref()
					.into_iter()
					.flat_map(|display| display.split(' '))
					.any(|directive| directive == key)
			};

			if !display_directive("hidden") {
				let value = if display_directive("sensitive") {
					quote! { "***********" }
				} else {
					quote! { format_args!("{:?}", self.#ident) }
				};

				let name = ident.to_string();
				summary.push(quote! {
					writeln!(out, "| {} | {} |", #name, #value)?;
				});
			}
		}
	}

	if emit && let Some(footer) = settings.get("footer") {
		section_buf.push_str(footer);
	}

	if emit {
		append_section(filename, truncate, section_buf.as_bytes());
	}

	let struct_name = &input.ident;
	let display = quote! {
		impl std::fmt::Display for #struct_name {
			fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
				writeln!(out, "| name | value |")?;
				writeln!(out, "| :--- | :---  |")?;
				#( #summary )*
				Ok(())
			}
		}
	};

	Ok(display)
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

fn get_default(field: &Field) -> Option<String> {
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

		let Some(arg) = Punctuated::<Meta, syn::Token![,]>::parse_terminated
			.parse(tokens.clone().into())
			.ok()?
			.into_iter()
			.next()
		else {
			continue;
		};

		match arg {
			| Meta::NameValue(MetaNameValue {
				value: Expr::Lit(ExprLit { lit: Lit::Str(str), .. }),
				..
			}) => {
				match str.value().as_str() {
					| "HashSet::new" | "Vec::new" | "RegexSet::empty" => Some("[]".to_owned()),
					| "true_fn" => return Some("true".to_owned()),
					| _ => return None,
				};
			},
			| Meta::Path { .. } => return Some("false".to_owned()),
			| _ => return None,
		}
	}

	None
}

fn example_value(field: &Field) -> String {
	get_doc_comment_line(field, "config-example")
		.or_else(|| get_doc_comment_line(field, "default"))
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
	use syn::parse_quote;

	use super::{example_value, get_doc_comment};

	#[test]
	fn config_example_overrides_default_for_generated_example_value() {
		let field: syn::Field = parse_quote! {
			#[doc = "config-example: from example"]
			#[doc = "default: from default"]
			name: String
		};

		assert_eq!(example_value(&field), "from example");
	}

	#[test]
	fn config_example_is_hidden_from_emitted_comments() {
		let field: syn::Field = parse_quote! {
			#[doc = "visible setting docs"]
			#[doc = "config-example: hidden value"]
			name: String
		};

		let comment = get_doc_comment(&field).expect("visible comment to be emitted");
		assert!(comment.contains("#visible setting docs"));
		assert!(!comment.contains("config-example:"));
	}
}
