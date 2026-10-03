//! Markup shared by browser-facing pages.
//!
//! The account pages and the single sign-on confirmation page render with the
//! same head, so its links stay identical between them.

/// Head elements for the account and sign-in pages.
///
/// Declares the character set and viewport and links the shared account
/// stylesheet.
pub(crate) static ACCOUNT_HEAD: &str = r#"
	<meta charset="UTF-8">
	<meta name="viewport" content="width=device-width, initial-scale=1">
	<link rel="stylesheet" href="/_tuwunel/oidc/account.css">
"#;
