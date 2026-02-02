use std::time::Duration;

use axum_server::Handle;

#[derive(Clone)]
pub(crate) struct ServerHandle {
	pub(crate) handle_ip: Handle<std::net::SocketAddr>,
	#[cfg(unix)]
	pub(crate) handle_unix: Handle<std::os::unix::net::SocketAddr>,
}

impl ServerHandle {
	pub(crate) fn new() -> Self {
		Self {
			handle_ip: Handle::<std::net::SocketAddr>::new(),
			#[cfg(unix)]
			handle_unix: Handle::<std::os::unix::net::SocketAddr>::new(),
		}
	}

	pub(crate) fn graceful_shutdown(&self, duration: Option<Duration>) {
		self.handle_ip.graceful_shutdown(duration);

		#[cfg(unix)]
		self.handle_unix.graceful_shutdown(duration);
	}
}
