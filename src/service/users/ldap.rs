#![cfg(feature = "ldap")]

use std::{collections::HashMap, time::Duration};

use ldap3::{
	LdapConnAsync, LdapConnSettings, Scope, SearchEntry, SearchOptions, dn_escape, ldap_escape,
};
use ruma::UserId;
use tokio::fs::read as read_file;
use tuwunel_core::{Result, debug, err, error, implement, result::LogErr, trace};

/// Cap LDAP connection setup so a hung directory cannot pin a login attempt.
const CONN_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap a directory search (in seconds) so a broad filter cannot make one login
/// scan the whole subtree unbounded.
const SEARCH_TIMELIMIT: i32 = 10;

/// Performs a LDAP search for the given user.
///
/// Returns the list of matching users, with a boolean for each result set
/// to true if the user is an admin.
#[implement(super::Service)]
pub async fn search_ldap(&self, user_id: &UserId) -> Result<Vec<(String, bool)>> {
	let localpart = user_id.localpart().to_owned();
	let lowercased_localpart = localpart.to_lowercase();

	let config = &self.services.server.config.ldap;
	let uri = config
		.uri
		.as_ref()
		.ok_or_else(|| err!(Ldap(error!("LDAP URI is not configured."))))?;

	if uri.scheme().starts_with("ldaps") {
		self.services.globals.init_rustls_provider()?;
	}

	debug!(?uri, "LDAP creating connection...");
	let (conn, mut ldap) = LdapConnAsync::with_settings(
		LdapConnSettings::new().set_conn_timeout(CONN_TIMEOUT),
		uri.as_str(),
	)
	.await
	.map_err(|e| {
		error!(?user_id, %e, "LDAP connection setup error");
		err!(Ldap("LDAP connection failed"))
	})?;

	let driver = self.services.server.runtime().spawn(async move {
		match conn.drive().await {
			| Err(e) => error!("LDAP connection error: {e}"),
			| Ok(()) => debug!("LDAP connection completed."),
		}
	});

	match (&config.bind_dn, &config.bind_password_file) {
		| (Some(bind_dn), Some(bind_password_file)) => {
			let bind_pw = String::from_utf8(read_file(bind_password_file).await?)?;

			ldap.simple_bind(bind_dn, bind_pw.trim())
				.await
				.and_then(ldap3::LdapResult::success)
				.map_err(|e| {
					error!(%e, "LDAP bind error");
					err!(Ldap("LDAP bind failed"))
				})?;
		},
		| (..) => {},
	}

	let attr = [&config.uid_attribute, &config.name_attribute];

	let escaped_localpart = ldap_escape(&lowercased_localpart);

	let user_filter = &config
		.filter
		.replace("{username}", &escaped_localpart);

	ldap.with_search_options(SearchOptions::new().timelimit(SEARCH_TIMELIMIT));

	let (entries, _result) = ldap
		.search(&config.base_dn, Scope::Subtree, user_filter, &attr)
		.await
		.and_then(ldap3::SearchResult::success)
		.inspect(|(entries, result)| trace!(?entries, ?result, "LDAP Search"))
		.map_err(|e| {
			error!(?attr, ?user_filter, %e, "LDAP search error");
			err!(Ldap("LDAP search failed"))
		})?;

	let mut dns: HashMap<String, bool> = entries
		.into_iter()
		.filter_map(|entry| {
			let search_entry = SearchEntry::construct(entry);
			debug!(?search_entry, "LDAP search entry");
			search_entry
				.attrs
				.get(&config.uid_attribute)
				.into_iter()
				.chain(search_entry.attrs.get(&config.name_attribute))
				.any(|ids| ids.contains(&localpart) || ids.contains(&lowercased_localpart))
				.then_some((search_entry.dn, false))
		})
		.collect();

	if !config.admin_filter.is_empty() {
		let admin_base_dn = if config.admin_base_dn.is_empty() {
			&config.base_dn
		} else {
			&config.admin_base_dn
		};

		let admin_filter = &config
			.admin_filter
			.replace("{username}", &escaped_localpart);

		ldap.with_search_options(SearchOptions::new().timelimit(SEARCH_TIMELIMIT));

		let (admin_entries, _result) = ldap
			.search(admin_base_dn, Scope::Subtree, admin_filter, &attr)
			.await
			.and_then(ldap3::SearchResult::success)
			.inspect(|(entries, result)| trace!(?entries, ?result, "LDAP Admin Search"))
			.map_err(|e| {
				error!(?attr, ?admin_filter, %e, "LDAP admin search error");
				err!(Ldap("LDAP admin search failed"))
			})?;

		dns.extend(admin_entries.into_iter().filter_map(|entry| {
			let search_entry = SearchEntry::construct(entry);
			debug!(?search_entry, "LDAP search entry");
			search_entry
				.attrs
				.get(&config.uid_attribute)
				.into_iter()
				.chain(search_entry.attrs.get(&config.name_attribute))
				.any(|ids| ids.contains(&localpart) || ids.contains(&lowercased_localpart))
				.then_some((search_entry.dn, true))
		}));
	}

	ldap.unbind().await.map_err(|e| {
		error!(%e, "LDAP unbind error");
		err!(Ldap("LDAP unbind failed"))
	})?;

	driver.await.log_err().ok();

	Ok(dns.drain().collect())
}

#[implement(super::Service)]
pub async fn auth_ldap(&self, user_dn: &str, password: &str) -> Result {
	// An empty password performs an unauthenticated bind (RFC 4513 5.1.2).
	if password.trim().is_empty() {
		return Err(err!(Request(Forbidden(debug_error!(
			"LDAP authentication error: empty password"
		)))));
	}

	let config = &self.services.server.config.ldap;
	let uri = config
		.uri
		.as_ref()
		.ok_or_else(|| err!(Ldap(error!("LDAP URI is not configured."))))?;

	if uri.scheme().starts_with("ldaps") {
		self.services.globals.init_rustls_provider()?;
	}

	debug!(?uri, "LDAP creating connection...");
	let (conn, mut ldap) = LdapConnAsync::with_settings(
		LdapConnSettings::new().set_conn_timeout(CONN_TIMEOUT),
		uri.as_str(),
	)
	.await
	.map_err(|e| {
		error!(?user_dn, %e, "LDAP connection setup error");
		err!(Ldap("LDAP connection failed"))
	})?;

	let driver = self.services.server.runtime().spawn(async move {
		match conn.drive().await {
			| Err(e) => error!("LDAP connection error: {e}"),
			| Ok(()) => debug!("LDAP connection completed."),
		}
	});

	ldap.simple_bind(user_dn, password)
		.await
		.and_then(ldap3::LdapResult::success)
		.map_err(|e| {
			debug!(%e, "LDAP authentication error");
			err!(Request(Forbidden("Invalid username or password.")))
		})?;

	ldap.unbind().await.map_err(|e| {
		error!(%e, "LDAP unbind error");
		err!(Ldap("LDAP unbind failed"))
	})?;

	driver.await.log_err().ok();

	Ok(())
}

/// Builds the user bind DN by substituting the escaped localpart into the
/// configured `bind_dn` template, or `None` when no `{username}` template is
/// set.
#[implement(super::Service)]
#[must_use]
pub fn ldap_bind_dn(&self, localpart: &str) -> Option<String> {
	self.services
		.server
		.config
		.ldap
		.bind_dn
		.as_ref()
		.filter(|template| template.contains("{username}"))
		.map(|template| template.replace("{username}", &dn_escape(localpart)))
}
