use std::{
	ffi::{CString, OsString},
	time::SystemTime,
};

use super::{Library, Symbol, canary, new, path};
use crate::{Result, error};

pub struct Module {
	handle: Option<Library>,
	loaded: SystemTime,
	path: OsString,
}

impl Module {
	pub fn from_name(name: &str) -> Result<Self> {
		Self::from_path(path::from_name(name)?)
	}

	pub fn from_path(path: OsString) -> Result<Self> {
		Ok(Self {
			handle: Some(new::from_path(&path)?),
			loaded: SystemTime::now(),
			path,
		})
	}

	pub fn unload(&mut self) {
		canary::prepare();
		self.close();
		if !canary::check_and_reset() {
			let name = self.name().expect("Module is named");
			error!("Module {name:?} is stuck and failed to unload.");
		}
	}

	pub(crate) fn close(&mut self) {
		if let Some(handle) = self.handle.take() {
			handle.close().expect("Module handle closed");
		}
	}

	pub fn get<Prototype>(&self, name: &str) -> Result<Symbol<Prototype>> {
		let cname = CString::new(name.to_owned()).expect("terminated string from provided name");
		let handle = self
			.handle
			.as_ref()
			.expect("backing library loaded by this instance");
		// SAFETY: Calls dlsym(3) on unix platforms. This might not have to be unsafe
		// if wrapped in libloading with_dlerror().
		let sym = unsafe { handle.get::<Prototype>(cname.as_bytes()) };
		let sym = sym.expect("symbol found; binding successful");

		Ok(sym)
	}

	pub fn deleted(&self) -> Result<bool> {
		let mtime = path::mtime(self.path())?;
		let res = mtime > self.loaded;

		Ok(res)
	}

	pub fn name(&self) -> Result<String> {
		path::to_name(self.path())
	}

	#[must_use]
	pub fn path(&self) -> &OsString {
		&self.path
	}
}

impl Drop for Module {
	fn drop(&mut self) {
		if self.handle.is_some() {
			self.unload();
		}
	}
}
