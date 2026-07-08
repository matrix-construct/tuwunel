use futures::StreamExt;
use ruma::UserId;
use tuwunel_core::{Result, utils};
use tuwunel_database::Deserialized;

use super::{PASSWORD_SENTINEL, Service};

/// Why deactivations carry a reason. Fork-only: neither Synapse nor upstream
/// Tuwunel has this concept.
///
/// Synapse models deactivation as a bare `users.deactivated` flag (plus the
/// separate, admin-imposed MSC3823 `suspended` flag) and stores no reason
/// anywhere. It hard-blocks a deactivated account from completing SSO login
/// (`auth.py` renders the `sso_account_deactivated_template` 403 page), and
/// its admin-only `activate_account` expects a password hash to be set
/// afterwards — which passwordless SSO accounts don't have. Upstream Tuwunel
/// likewise stores no reason; since v1.8.1 an admin can reactivate a
/// passwordless user via `PASSWORD_SENTINEL`, but there is no self-service
/// path in either server.
///
/// This fork adds self-service reactivation: a *self*-deactivated SSO-origin
/// account is restored when the same IdP identity returns
/// (`maybe_reactivate_deactivated_sso`). Persisting who initiated the
/// deactivation is what makes that safe — admin deactivations must stay
/// final, or a banned user could resurrect their account by simply logging
/// in again. The reason is therefore a *required* parameter of
/// `deactivate_account`: every new call site (typically new upstream
/// Synapse-admin endpoints appearing at each rebase) fails to compile until
/// it explicitly classifies itself as self-service or administrative,
/// instead of inheriting a default.
///
/// Deliberately not upstreamed: reversible deactivation contradicts the
/// semantics Synapse established (its closest concept, MSC3823 suspension,
/// is admin-imposed and different), so upstreaming would effectively require
/// an MSC first. Recorded here so future rebases don't have to re-derive
/// this from the Synapse sources; see also the design note in
/// `FORK_CHANGES.md`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeactivationReason {
	SelfService,
	Admin,
}

impl DeactivationReason {
	const fn as_str(self) -> &'static str {
		match self {
			| Self::SelfService => "self",
			| Self::Admin => "admin",
		}
	}
}

fn can_reactivate_deactivated_sso(reason: Option<&str>) -> bool { matches!(reason, Some("self")) }

impl Service {
	/// Reactivate a deactivated local SSO account.
	///
	/// This is used for users who self-deactivated and later return through the
	/// same SSO identity. The account remains the same MXID, but can
	/// authenticate again.
	pub async fn maybe_reactivate_deactivated_sso(&self, user_id: &UserId) -> Result<bool> {
		if !self.services.globals.user_is_local(user_id) {
			return Ok(false);
		}

		if !self.is_deactivated(user_id).await? {
			return Ok(false);
		}

		let Ok(origin) = self.origin(user_id).await else {
			return Ok(false);
		};

		if origin != "sso" {
			return Ok(false);
		}

		if !can_reactivate_deactivated_sso(self.deactivation_reason(user_id).await.as_deref()) {
			return Ok(false);
		}

		self.set_password(user_id, Some(PASSWORD_SENTINEL))
			.await?;
		Ok(true)
	}

	pub async fn deactivation_reason(&self, user_id: &UserId) -> Option<String> {
		self.db
			.userid_deactivation_reason
			.get(user_id)
			.await
			.deserialized()
			.ok()
	}

	#[inline]
	pub fn set_deactivation_reason(&self, user_id: &UserId, reason: DeactivationReason) {
		self.db
			.userid_deactivation_reason
			.insert(user_id, reason.as_str());
	}

	/// Compatibility repair for legacy SSO users whose origin was accidentally
	/// rewritten to `password` during account creation.
	pub async fn maybe_repair_legacy_sso_origin(&self, user_id: &UserId) -> bool {
		let oauth_sessions = self
			.services
			.oauth
			.sessions
			.get_sess_id_by_user(user_id);
		futures::pin_mut!(oauth_sessions);

		let Some(Ok(_)) = oauth_sessions.next().await else {
			return false;
		};

		let Ok(origin) = self.origin(user_id).await else {
			return false;
		};

		if origin != "password" {
			return false;
		}

		let Ok(password_hash) = self.password_hash(user_id).await else {
			return false;
		};

		if password_hash.is_empty() {
			return false;
		}

		let is_sentinel_password = password_hash == PASSWORD_SENTINEL
			|| utils::hash::verify_password(PASSWORD_SENTINEL, &password_hash).is_ok();
		if !is_sentinel_password {
			return false;
		}

		self.set_origin(user_id, "sso");
		true
	}

	#[cfg(test)]
	fn test_can_reactivate_deactivated_sso(reason: Option<&str>) -> bool {
		can_reactivate_deactivated_sso(reason)
	}
}

#[cfg(test)]
mod tests {
	use std::{
		fs,
		path::{Path, PathBuf},
		sync::{
			Arc,
			atomic::{AtomicU64, Ordering},
		},
	};

	use ruma::user_id;
	use tracing::subscriber::NoSubscriber;
	use tuwunel_core::{
		Server,
		config::Config,
		log::{Logging, capture::State as CaptureState},
		metrics::Metrics,
	};

	use super::{super::PASSWORD_SENTINEL, DeactivationReason, Service};
	use crate::Services;

	static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

	#[test]
	fn sso_reactivation_requires_self_deactivation_reason() {
		assert!(Service::test_can_reactivate_deactivated_sso(Some("self")));
		assert!(!Service::test_can_reactivate_deactivated_sso(None));
		assert!(!Service::test_can_reactivate_deactivated_sso(Some("admin")));
		assert!(!Service::test_can_reactivate_deactivated_sso(Some("unknown")));
	}

	#[tokio::test]
	async fn self_deactivated_sso_account_reactivates() {
		let temp_dir = unique_temp_dir();
		let services = open_services(&temp_dir).await;
		let user_id = user_id!("@alice:example.com");

		services
			.users
			.create(user_id, Some(PASSWORD_SENTINEL), Some("sso"))
			.await
			.expect("create SSO user");
		services
			.users
			.deactivate_account(user_id, DeactivationReason::SelfService)
			.await
			.expect("deactivate SSO user");

		let reactivated = services
			.users
			.maybe_reactivate_deactivated_sso(user_id)
			.await
			.expect("reactivation check should succeed");

		assert!(reactivated, "self-deactivated SSO user should reactivate");
		assert!(
			!services
				.users
				.is_deactivated(user_id)
				.await
				.expect("deactivation state"),
			"user should no longer be deactivated"
		);
		assert_eq!(
			services
				.users
				.origin(user_id)
				.await
				.expect("origin"),
			"sso",
			"reactivation should preserve SSO origin"
		);
		assert_eq!(
			services
				.users
				.password_hash(user_id)
				.await
				.expect("password hash"),
			PASSWORD_SENTINEL,
			"reactivation should restore the sentinel password"
		);

		cleanup_temp_dir(&temp_dir);
	}

	#[tokio::test]
	async fn admin_deactivated_sso_account_does_not_reactivate() {
		let temp_dir = unique_temp_dir();
		let services = open_services(&temp_dir).await;
		let user_id = user_id!("@alice:example.com");

		services
			.users
			.create(user_id, Some(PASSWORD_SENTINEL), Some("sso"))
			.await
			.expect("create SSO user");
		services
			.users
			.deactivate_account(user_id, DeactivationReason::Admin)
			.await
			.expect("deactivate SSO user");

		let reactivated = services
			.users
			.maybe_reactivate_deactivated_sso(user_id)
			.await
			.expect("reactivation check should succeed");

		assert!(!reactivated, "admin-deactivated SSO user must stay deactivated");
		assert!(
			services
				.users
				.is_deactivated(user_id)
				.await
				.expect("deactivation state"),
			"user should remain deactivated"
		);

		cleanup_temp_dir(&temp_dir);
	}

	#[tokio::test]
	async fn self_deactivated_password_account_does_not_reactivate_as_sso() {
		let temp_dir = unique_temp_dir();
		let services = open_services(&temp_dir).await;
		let user_id = user_id!("@alice:example.com");

		services
			.users
			.create(user_id, Some("password"), Some("password"))
			.await
			.expect("create password user");
		services
			.users
			.deactivate_account(user_id, DeactivationReason::SelfService)
			.await
			.expect("deactivate password user");

		let reactivated = services
			.users
			.maybe_reactivate_deactivated_sso(user_id)
			.await
			.expect("reactivation check should succeed");

		assert!(!reactivated, "password-origin user must not reactivate via SSO path");
		assert!(
			services
				.users
				.is_deactivated(user_id)
				.await
				.expect("deactivation state"),
			"user should remain deactivated"
		);
		assert_eq!(
			services
				.users
				.origin(user_id)
				.await
				.expect("origin"),
			"password",
			"SSO reactivation check should not change user origin"
		);

		cleanup_temp_dir(&temp_dir);
	}

	async fn open_services(temp_dir: &Path) -> Arc<Services> {
		let db_path = temp_dir.join("db");
		let config_path = temp_dir.join("tuwunel.toml");

		fs::create_dir_all(temp_dir).expect("create test temp dir");
		let config_contents = format!(
			r#"[global]
server_name = "example.com"
database_path = "{}"
"#,
			db_path.display(),
		);
		fs::write(&config_path, config_contents).expect("write test config");

		let figment = Config::load(std::iter::once(config_path.as_path())).expect("load config");
		let config = Config::new(&figment).expect("parse config");
		let log = Logging {
			reload: Default::default(),
			capture: Arc::new(CaptureState::new()),
			subscriber: Arc::new(NoSubscriber::new()),
		};
		let runtime = tokio::runtime::Handle::current();
		let metrics = Metrics::new(Some(&runtime));
		let server = Arc::new(Server::new(config, Some(&runtime), log, metrics));

		Services::build(server)
			.await
			.expect("build services")
	}

	fn unique_temp_dir() -> PathBuf {
		let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
		let pid = std::process::id();
		let path = std::env::temp_dir().join(format!("tuwunel-users-{pid}-{id}"));
		fs::create_dir_all(&path).expect("create unique test dir");
		path
	}

	fn cleanup_temp_dir(path: &Path) { let _: std::io::Result<()> = fs::remove_dir_all(path); }
}
