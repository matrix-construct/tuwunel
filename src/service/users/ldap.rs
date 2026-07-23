#![cfg(feature = "ldap")]

use std::collections::HashMap;

use ldap3::{LdapConnAsync, Scope, SearchEntry, ldap_escape};
use ruma::UserId;
use tuwunel_core::{Result, debug, err, error, implement, result::LogErr, trace};

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
	let (conn, mut ldap) = LdapConnAsync::new(uri.as_str())
		.await
		.map_err(|e| err!(Ldap(error!(?user_id, "LDAP connection setup error: {e}"))))?;

	let driver = self.services.server.runtime().spawn(async move {
		match conn.drive().await {
			| Err(e) => error!("LDAP connection error: {e}"),
			| Ok(()) => debug!("LDAP connection completed."),
		}
	});

	match (&config.bind_dn, &config.bind_password_file) {
		| (Some(bind_dn), Some(bind_password_file)) => {
			let bind_pw = String::from_utf8(std::fs::read(bind_password_file)?)?;
			ldap.simple_bind(bind_dn, bind_pw.trim())
				.await
				.and_then(ldap3::LdapResult::success)
				.map_err(|e| err!(Ldap(error!("LDAP bind error: {e}"))))?;
		},
		| (..) => {},
	}

	let attr = [&config.uid_attribute, &config.name_attribute];

	let escaped_localpart = ldap_escape(&lowercased_localpart);

	let user_filter = &config
		.filter
		.replace("{username}", &escaped_localpart);

	let (entries, _result) = ldap
		.search(&config.base_dn, Scope::Subtree, user_filter, &attr)
		.await
		.and_then(ldap3::SearchResult::success)
		.inspect(|(entries, result)| trace!(?entries, ?result, "LDAP Search"))
		.map_err(|e| err!(Ldap(error!(?attr, ?user_filter, "LDAP search error: {e}"))))?;

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

		let (admin_entries, _result) = ldap
			.search(admin_base_dn, Scope::Subtree, admin_filter, &attr)
			.await
			.and_then(ldap3::SearchResult::success)
			.inspect(|(entries, result)| trace!(?entries, ?result, "LDAP Admin Search"))
			.map_err(|e| {
				err!(Ldap(error!(?attr, ?admin_filter, "Ldap admin search error: {e}")))
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

	ldap.unbind()
		.await
		.map_err(|e| err!(Ldap(error!("LDAP unbind error: {e}"))))?;

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
	let (conn, mut ldap) = LdapConnAsync::new(uri.as_str())
		.await
		.map_err(|e| err!(Ldap(error!(?user_dn, "LDAP connection setup error: {e}"))))?;

	let driver = self.services.server.runtime().spawn(async move {
		match conn.drive().await {
			| Err(e) => error!("LDAP connection error: {e}"),
			| Ok(()) => debug!("LDAP connection completed."),
		}
	});

	ldap.simple_bind(user_dn, password)
		.await
		.and_then(ldap3::LdapResult::success)
		.map_err(|e| err!(Request(Forbidden(debug_error!("LDAP authentication error: {e}")))))?;

	ldap.unbind()
		.await
		.map_err(|e| err!(Ldap(error!("LDAP unbind error: {e}"))))?;

	driver.await.log_err().ok();

	Ok(())
}
