use clap::Subcommand;
use futures::{StreamExt, TryStreamExt};
use ruma::OwnedUserId;
use tuwunel_core::{
	Err, Result, apply,
	either::{Either, Left, Right},
	err,
	itertools::Itertools,
	utils::stream::{IterStream, ReadyExt},
};
use tuwunel_service::oauth::{Provider, ProviderId, SessionId};

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

	/// List users associated with any OAuth session
	ListUsers,

	/// List session ID's
	ListSessions {
		#[arg(long)]
		user: Option<OwnedUserId>,
	},

	/// Show active configuration of a provider.
	ShowProvider {
		id: ProviderId,

		#[arg(long)]
		config: bool,
	},

	/// Show session state
	ShowSession {
		id: SessionId,
	},

	/// Show user sessions
	ShowUser {
		user_id: OwnedUserId,
	},

	/// Token introspection request to provider.
	TokenInfo {
		id: SessionId,
	},

	/// Revoke token for user_id or sess_id.
	Revoke {
		#[arg(value_parser = session_or_user_id)]
		id: Either<SessionId, OwnedUserId>,
	},

	/// Remove oauth state (DANGER!)
	Delete {
		#[arg(value_parser = session_or_user_id)]
		id: Either<SessionId, OwnedUserId>,

		#[arg(long)]
		force: bool,
	},
}

type SessionOrUserId = Either<SessionId, OwnedUserId>;

fn session_or_user_id(input: &str) -> Result<SessionOrUserId> {
	OwnedUserId::parse(input)
		.map(Right)
		.or_else(|_| Ok(Left(input.to_owned())))
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
		.values()
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
pub(super) async fn oauth_list_sessions(&self, user_id: Option<OwnedUserId>) -> Result {
	if let Some(user_id) = user_id.as_deref() {
		return self
			.services
			.oauth
			.sessions
			.get_sess_id_by_user(user_id)
			.map_ok(|id| format!("{id}\n"))
			.try_for_each(async |id: String| self.write_str(&id).await)
			.await;
	}

	self.services
		.oauth
		.sessions
		.stream()
		.ready_filter_map(|sess| sess.sess_id)
		.map(|sess_id| format!("{sess_id:?}\n"))
		.for_each(async |id: String| {
			self.write_str(&id).await.ok();
		})
		.await;

	Ok(())
}

#[admin_command]
pub(super) async fn oauth_show_provider(&self, id: ProviderId, config: bool) -> Result {
	if config {
		let config = self.services.oauth.providers.get_config(&id)?;

		self.write_str(&format!("{config:#?}\n")).await?;
		return Ok(());
	}

	let provider = self.services.oauth.providers.get(&id).await?;

	self.write_str(&format!("{provider:#?}\n")).await
}

#[admin_command]
pub(super) async fn oauth_show_session(&self, id: SessionId) -> Result {
	let session = self.services.oauth.sessions.get(&id).await?;

	self.write_str(&format!("{session:#?}\n")).await
}

#[admin_command]
pub(super) async fn oauth_show_user(&self, user_id: OwnedUserId) -> Result {
	self.services
		.oauth
		.sessions
		.get_sess_id_by_user(&user_id)
		.try_for_each(async |id| {
			let session = self.services.oauth.sessions.get(&id).await?;

			self.write_str(&format!("{session:#?}\n")).await
		})
		.await
}

#[admin_command]
pub(super) async fn oauth_token_info(&self, id: SessionId) -> Result {
	let session = self.services.oauth.sessions.get(&id).await?;

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
		.await?;

	self.write_str(&format!("{tokeninfo:#?}\n")).await
}

#[admin_command]
pub(super) async fn oauth_revoke(&self, id: SessionOrUserId) -> Result {
	match id {
		| Left(sess_id) => {
			let session = self.services.oauth.sessions.get(&sess_id).await?;

			let provider = self
				.services
				.oauth
				.sessions
				.provider(&session)
				.await?;

			self.services
				.oauth
				.revoke_token((&provider, &session))
				.await
				.ok();
		},
		| Right(user_id) =>
			self.services
				.oauth
				.revoke_user_tokens(&user_id)
				.await,
	}

	self.write_str("revoked").await
}

#[admin_command]
pub(super) async fn oauth_delete(&self, id: SessionOrUserId, force: bool) -> Result {
	if !force {
		return Err!(
			"Deleting these records can cause registration conflicts. Use --force to be sure."
		);
	}

	match id {
		| Left(sess_id) => {
			self.services
				.oauth
				.sessions
				.delete(&sess_id)
				.await;
		},
		| Right(user_id) => {
			self.services
				.oauth
				.delete_user_sessions(&user_id)
				.await;
		},
	}

	self.write_str("deleted any oauth state for {id}")
		.await
}
