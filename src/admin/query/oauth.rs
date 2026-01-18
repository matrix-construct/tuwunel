use clap::Subcommand;
use futures::{StreamExt, TryStreamExt};
use ruma::OwnedUserId;
use tuwunel_core::{Err, Result, apply, err, itertools::Itertools, utils::stream::IterStream};
use tuwunel_service::{
	Services,
	oauth::{Provider, Session},
};

use crate::{admin_command, admin_command_dispatch};

#[admin_command_dispatch(handler_prefix = "oauth")]
#[derive(Debug, Subcommand)]
/// Query OAuth service state
pub(crate) enum OauthCommand {
	/// Associate existing user with future authorization claims.
	Associate {
		/// ID of configured provider to listen on.
		provider: String,

		/// MXID of local user to associate.
		user_id: OwnedUserId,

		/// List of claims to match in key=value format.
		#[arg(long, required = true)]
		claim: Vec<String>,
	},

	/// List configured OAuth providers.
	ListProviders,

	/// List users associated with an OAuth provider
	ListUsers,

	/// Show active configuration of a provider.
	ShowProvider {
		id: String,

		#[arg(long)]
		config: bool,
	},

	/// Show session state
	ShowSession {
		id: String,
	},

	/// Token introspection request to provider.
	TokenInfo {
		id: String,
	},

	/// Revoke token for user_id or sess_id.
	Revoke {
		id: String,
	},

	/// Remove oauth state (DANGER!)
	Remove {
		id: String,

		#[arg(long)]
		force: bool,
	},
}

#[admin_command]
pub(super) async fn oauth_associate(
	&self,
	provider: String,
	user_id: OwnedUserId,
	claim: Vec<String>,
) -> Result {
	if !self.services.globals.user_is_local(&user_id) {
		return Err!(Request(NotFound("User {user_id:?} does not belong to this server.")));
	}

	if !self.services.users.exists(&user_id).await {
		return Err!(Request(NotFound("User {user_id:?} is not registered")));
	}

	let provider = self
		.services
		.oauth
		.providers
		.get(&provider)
		.await?;

	let claim = claim
		.iter()
		.map(|kv| {
			let (key, val) = kv
				.split_once('=')
				.ok_or_else(|| err!("Missing '=' in --claim {kv}=???"))?;

			if !key.is_empty() && !val.is_empty() {
				Ok((key, val))
			} else {
				Err!("Missing key or value in --claim=key=value argument")
			}
		})
		.map_ok(apply!(2, ToOwned::to_owned))
		.collect::<Result<_>>()?;

	let _replaced = self
		.services
		.oauth
		.sessions
		.set_user_association_pending(provider.id(), &user_id, claim);

	Ok(())
}

#[admin_command]
pub(super) async fn oauth_list_providers(&self) -> Result {
	self.services
		.config
		.identity_provider
		.iter()
		.try_stream()
		.map_ok(Provider::id)
		.map_ok(|id| format!("{id}\n"))
		.try_for_each(async |id| self.write_str(&id).await)
		.await
}

#[admin_command]
pub(super) async fn oauth_list_users(&self) -> Result {
	self.services
		.oauth
		.sessions
		.users()
		.map(|id| format!("{id}\n"))
		.map(Ok)
		.try_for_each(async |id: String| self.write_str(&id).await)
		.await
}

#[admin_command]
pub(super) async fn oauth_show_provider(&self, id: String, config: bool) -> Result {
	if config {
		let config = self.services.oauth.providers.get_config(&id)?;

		self.write_str(&format!("{config:#?}\n")).await?;
		return Ok(());
	}

	let provider = self.services.oauth.providers.get(&id).await?;

	self.write_str(&format!("{provider:#?}\n")).await
}

#[admin_command]
pub(super) async fn oauth_show_session(&self, id: String) -> Result {
	let session = find_session(self.services, &id).await?;

	self.write_str(&format!("{session:#?}\n")).await
}

#[admin_command]
pub(super) async fn oauth_token_info(&self, id: String) -> Result {
	let session = find_session(self.services, &id).await?;

	let provider = self
		.services
		.oauth
		.sessions
		.provider(&session)
		.await?;

	let tokeninfo = self
		.services
		.oauth
		.request_tokeninfo((&provider, &session))
		.await;

	self.write_str(&format!("{tokeninfo:#?}\n")).await
}

#[admin_command]
pub(super) async fn oauth_revoke(&self, id: String) -> Result {
	let session = find_session(self.services, &id).await?;

	let provider = self
		.services
		.oauth
		.sessions
		.provider(&session)
		.await?;

	self.services
		.oauth
		.revoke_token((&provider, &session))
		.await?;

	self.write_str("done").await
}

#[admin_command]
pub(super) async fn oauth_remove(&self, id: String, force: bool) -> Result {
	let session = find_session(self.services, &id).await?;

	let Some(sess_id) = session.sess_id else {
		return Err!("Missing sess_id in oauth Session state");
	};

	if !force {
		return Err!(
			"Deleting these records can cause registration conflicts. Use --force to be sure."
		);
	}

	self.services
		.oauth
		.sessions
		.delete(&sess_id)
		.await;

	self.write_str("done").await
}

async fn find_session(services: &Services, id: &str) -> Result<Session> {
	if let Ok(user_id) = OwnedUserId::parse(id) {
		services
			.oauth
			.sessions
			.get_by_user(&user_id)
			.await
	} else {
		services.oauth.sessions.get(id).await
	}
}
