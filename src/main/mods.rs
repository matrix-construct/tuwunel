#![cfg(all(tuwunel_mods, feature = "tuwunel_mods"))]

#[unsafe(no_link)]
extern crate tuwunel_service;

use std::{
	future::Future,
	pin::Pin,
	sync::{Arc, atomic::Ordering},
};

use tuwunel_core::{Error, Result, debug, error, mods};
use tuwunel_service::Services;

use crate::Server;

type StartFuncResult = Pin<Box<dyn Future<Output = Result<Arc<Services>>> + Send>>;
type StartFuncProto = fn(&Arc<tuwunel_core::Server>) -> StartFuncResult;

type RunFuncResult = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
type RunFuncProto = fn(&Arc<Services>) -> RunFuncResult;

type StopFuncResult = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
type StopFuncProto = fn(Arc<Services>) -> StopFuncResult;

const RESTART_THRESH: &str = "tuwunel_service";
const MODULE_NAMES: &[&str] = &[
	//"tuwunel_core",
	"tuwunel_database",
	"tuwunel_service",
	"tuwunel_api",
	"tuwunel_admin",
	"tuwunel_router",
];

#[cfg(panic_trap)]
tuwunel_core::mod_init! {{
	tuwunel_core::debug::set_panic_trap();
}}

pub(crate) async fn run(server: &Arc<Server>, starts: bool) -> Result<(bool, bool), Error> {
	let main_lock = server.mods.read().await;
	let main_mod = (*main_lock).last().expect("main module loaded");
	if starts {
		let start = main_mod.get::<StartFuncProto>("start")?;
		match start(&server.server).await {
			| Ok(services) => server.services.lock().await.insert(services),
			| Err(error) => {
				error!("Starting server: {error}");
				return Err(error);
			},
		};
	}
	server
		.server
		.stopping
		.store(false, Ordering::Release);
	let run = main_mod.get::<RunFuncProto>("run")?;
	if let Err(error) = run(server
		.services
		.lock()
		.await
		.as_ref()
		.expect("services initialized"))
	.await
	{
		error!("Running server: {error}");
		return Err(error);
	}
	let reloads = server
		.server
		.reloading
		.swap(false, Ordering::AcqRel);
	let stops = !reloads || stale(server).await? <= restart_thresh();
	let starts = reloads && stops;
	if stops {
		let stop = main_mod.get::<StopFuncProto>("stop")?;
		if let Err(error) = stop(
			server
				.services
				.lock()
				.await
				.take()
				.expect("services initialized"),
		)
		.await
		{
			error!("Stopping server: {error}");
			return Err(error);
		}
	}

	Ok((starts, reloads))
}

pub(crate) async fn open(server: &Arc<Server>) -> Result<usize, Error> {
	let mut mods_lock = server.mods.write().await;
	let mods: &mut Vec<mods::Module> = &mut mods_lock;
	debug!(
		available = %available(),
		loaded = %mods.len(),
		"Loading modules",
	);

	for (i, name) in MODULE_NAMES.iter().enumerate() {
		if mods.get(i).is_none() {
			mods.push(mods::Module::from_name(name)?);
		}
	}

	Ok(mods.len())
}

pub(crate) async fn close(server: &Arc<Server>, force: bool) -> Result<usize, Error> {
	let stale = stale_count(server).await;
	let mut mods_lock = server.mods.write().await;
	let mods: &mut Vec<mods::Module> = &mut mods_lock;
	debug!(
		available = %available(),
		loaded = %mods.len(),
		stale = %stale,
		force,
		"Unloading modules",
	);

	while mods.last().is_some() {
		let module = &mods.last().expect("module");
		if force || module.deleted()? {
			mods.pop();
		} else {
			break;
		}
	}

	Ok(mods.len())
}

async fn stale_count(server: &Arc<Server>) -> usize {
	let watermark = stale(server).await.unwrap_or(available());
	available().saturating_sub(watermark)
}

async fn stale(server: &Arc<Server>) -> Result<usize, Error> {
	let mods_lock = server.mods.read().await;
	let mods: &Vec<mods::Module> = &mods_lock;
	for (i, module) in mods.iter().enumerate() {
		if module.deleted()? {
			return Ok(i);
		}
	}

	Ok(mods.len())
}

fn restart_thresh() -> usize {
	MODULE_NAMES
		.iter()
		.position(|&name| name.ends_with(RESTART_THRESH))
		.unwrap_or(MODULE_NAMES.len())
}

const fn available() -> usize {
	MODULE_NAMES.len()
}
