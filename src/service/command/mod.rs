use std::sync::Arc;

use async_trait::async_trait;
use ruma::UserId;
use tuwunel_core::{Result, log::capture::EventData};

pub mod complete;
pub mod run;
pub mod run_matrix;

pub struct CommandResult {
	pub output: String,
	pub logs: Vec<EventData>,
	pub err: bool,
}

pub struct CompletionTree {
	pub name: String,
	pub nodes: Vec<Self>,
}

#[async_trait]
pub trait CommandSystem: Send + Sync {
	fn parse(&self, command_line: &str) -> Vec<String>;

	fn get_completion_tree(&self) -> CompletionTree;

	async fn process(
		&self,
		command_line: &[&str],
		input: &str,
		sender: Option<&UserId>,
	) -> Result<String>;
}

pub struct Service {
	services: Arc<crate::services::OnceServices>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self { services: args.services.clone() }))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}
