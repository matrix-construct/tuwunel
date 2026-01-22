use std::{collections::HashSet, fmt::Write as _, fs::OpenOptions, io::Write as _};

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{ToTokens, quote};
use syn::{
	Error, Expr, ExprLit, Field, Fields, FieldsNamed, ItemStruct, Lit, Meta, MetaList,
	MetaNameValue, Type, TypePath, parse::Parser, punctuated::Punctuated, spanned::Spanned,
};

use crate::{
	Result,
	utils::{get_simple_settings, is_cargo_build, is_cargo_test},
};

const UNDOCUMENTED: &str = "# This item is undocumented. Please contribute documentation for it.";

const HIDDEN: &[&str] = &["default", "display"];

#[expect(clippy::needless_pass_by_value)]
pub(super) fn example_generator(input: ItemStruct, args: &[Meta]) -> Result<TokenStream> {
	let write = is_cargo_build() && !is_cargo_test();
	let additional = generate_example(&input, args, write)?;

	Ok([input.to_token_stream(), additional]
		.into_iter()
		.collect::<TokenStream2>()
		.into())
}

#[allow(clippy::needless_pass_by_value)]
fn generate_example(input: &ItemStruct, args: &[Meta], write: bool) -> Result<TokenStream2> {
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

	let fopts = OpenOptions::new()
		.write(true)
		.create(section == "global")
		.truncate(section == "global")
		.append(section != "global")
		.clone();

	let mut file = write
		.then(|| {
			fopts.open(filename).map_err(|e| {
				let msg = format!("Failed to open file for config generation: {e}");
				Error::new(Span::call_site(), msg)
			})
		})
		.transpose()?;

	if let Some(file) = file.as_mut() {
		if let Some(header) = settings.get("header") {
			file.write_all(header.as_bytes())
				.expect("written to config file");
		}

		let pound = if section != "global" { "\n#" } else { "" };
		file.write_fmt(format_args!("\n\n{pound}[{section}]\n"))
			.expect("written to config file");
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

			let doc = if doc.ends_with('#') {
				format!("{doc}\n")
			} else {
				format!("{doc}\n#\n")
			};

			let default = get_doc_comment_line(field, "default")
				.or_else(|| get_default(field))
				.unwrap_or_default();

			let default = if !default.is_empty() {
				format!(" {default}")
			} else {
				default
			};

			if let Some(file) = file.as_mut() {
				file.write_fmt(format_args!("\n{doc}"))
					.expect("written to config file");

				file.write_fmt(format_args!("#{ident} ={default}\n"))
					.expect("written to config file");
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

	if let Some(file) = file.as_mut()
		&& let Some(footer) = settings.get("footer")
	{
		file.write_all(footer.as_bytes())
			.expect("written to config file");
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
			.iter()
			.next()
			.cloned()
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

fn get_doc_comment(field: &Field) -> Option<String> {
	let comment = get_doc_comment_full(field)?;

	let out = comment
		.lines()
		.filter(|line| {
			!HIDDEN.iter().any(|key| {
				line.trim().starts_with(key) && line.trim().chars().nth(key.len()) == Some(':')
			})
		})
		.fold(String::new(), |full, line| full + "#" + line + "\n");

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
