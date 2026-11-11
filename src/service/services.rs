use std::{fmt, sync::Arc};

use futures::{StreamExt, TryStreamExt};
use tokio::sync::Mutex;
use tuwunel_core::{
	Result, Server, debug, debug_info, implement, info, trace, utils::stream::IterStream, warn,
};
use tuwunel_database::Database;

pub(crate) use crate::OnceServices;
use crate::{
	account_data, admin, appservice, client, config, deactivate, emergency, federation, globals,
	key_backups,
	manager::Manager,
	media, membership, oauth, presence, pusher, registration_tokens, replication, resolver,
	rooms::{self, retention},
	sending, server_keys,
	service::{Args, Service},
	sync, transaction_ids, uiaa, users,
};

pub struct Services {
	pub account_data: Arc<account_data::Service>,
	pub admin: Arc<admin::Service>,
	pub appservice: Arc<appservice::Service>,
	pub config: Arc<config::Service>,
	pub client: Arc<client::Service>,
	pub emergency: Arc<emergency::Service>,
	pub globals: Arc<globals::Service>,
	pub key_backups: Arc<key_backups::Service>,
	pub media: Arc<media::Service>,
	pub presence: Arc<presence::Service>,
	pub pusher: Arc<pusher::Service>,
	pub resolver: Arc<resolver::Service>,
	pub alias: Arc<rooms::alias::Service>,
	pub auth_chain: Arc<rooms::auth_chain::Service>,
	pub delete: Arc<rooms::delete::Service>,
	pub directory: Arc<rooms::directory::Service>,
	pub event_handler: Arc<rooms::event_handler::Service>,
	pub lazy_loading: Arc<rooms::lazy_loading::Service>,
	pub metadata: Arc<rooms::metadata::Service>,
	pub pdu_metadata: Arc<rooms::pdu_metadata::Service>,
	pub read_receipt: Arc<rooms::read_receipt::Service>,
	pub search: Arc<rooms::search::Service>,
	pub short: Arc<rooms::short::Service>,
	pub spaces: Arc<rooms::spaces::Service>,
	pub state: Arc<rooms::state::Service>,
	pub state_accessor: Arc<rooms::state_accessor::Service>,
	pub state_cache: Arc<rooms::state_cache::Service>,
	pub state_compressor: Arc<rooms::state_compressor::Service>,
	pub threads: Arc<rooms::threads::Service>,
	pub timeline: Arc<rooms::timeline::Service>,
	pub typing: Arc<rooms::typing::Service>,
	pub federation: Arc<federation::Service>,
	pub sending: Arc<sending::Service>,
	pub server_keys: Arc<server_keys::Service>,
	pub sync: Arc<sync::Service>,
	pub transaction_ids: Arc<transaction_ids::Service>,
	pub uiaa: Arc<uiaa::Service>,
	pub users: Arc<users::Service>,
	pub membership: Arc<membership::Service>,
	pub deactivate: Arc<deactivate::Service>,
	pub oauth: Arc<oauth::Service>,
	pub retention: Arc<retention::Service>,
	pub registration_tokens: Arc<registration_tokens::Service>,
	pub replication: Arc<replication::Service>,

	manager: Mutex<Option<Arc<Manager>>>,
	pub server: Arc<Server>,
	pub db: Arc<Database>,
}

#[implement(Services)]
pub async fn build(server: Arc<Server>) -> Result<Arc<Self>> {
	Self::maybe_bootstrap_checkpoint(&server).await?;
	let db = Database::open(&server).await?;
	let services = Arc::new(OnceServices::default());
	let args = Args {
		db: &db,
		server: &server,
		services: &services,
	};

	let res = Arc::new(Self {
		account_data: account_data::Service::build(&args)?,
		admin: admin::Service::build(&args)?,
		appservice: appservice::Service::build(&args)?,
		resolver: resolver::Service::build(&args)?,
		client: client::Service::build(&args)?,
		config: config::Service::build(&args)?,
		emergency: emergency::Service::build(&args)?,
		globals: globals::Service::build(&args)?,
		key_backups: key_backups::Service::build(&args)?,
		media: media::Service::build(&args)?,
		presence: presence::Service::build(&args)?,
		pusher: pusher::Service::build(&args)?,
		alias: rooms::alias::Service::build(&args)?,
		auth_chain: rooms::auth_chain::Service::build(&args)?,
		delete: rooms::delete::Service::build(&args)?,
		directory: rooms::directory::Service::build(&args)?,
		event_handler: rooms::event_handler::Service::build(&args)?,
		lazy_loading: rooms::lazy_loading::Service::build(&args)?,
		metadata: rooms::metadata::Service::build(&args)?,
		pdu_metadata: rooms::pdu_metadata::Service::build(&args)?,
		read_receipt: rooms::read_receipt::Service::build(&args)?,
		search: rooms::search::Service::build(&args)?,
		short: rooms::short::Service::build(&args)?,
		spaces: rooms::spaces::Service::build(&args)?,
		state: rooms::state::Service::build(&args)?,
		state_accessor: rooms::state_accessor::Service::build(&args)?,
		state_cache: rooms::state_cache::Service::build(&args)?,
		state_compressor: rooms::state_compressor::Service::build(&args)?,
		threads: rooms::threads::Service::build(&args)?,
		timeline: rooms::timeline::Service::build(&args)?,
		typing: rooms::typing::Service::build(&args)?,
		federation: federation::Service::build(&args)?,
		sending: sending::Service::build(&args)?,
		server_keys: server_keys::Service::build(&args)?,
		sync: sync::Service::build(&args)?,
		transaction_ids: transaction_ids::Service::build(&args)?,
		uiaa: uiaa::Service::build(&args)?,
		users: users::Service::build(&args)?,
		membership: membership::Service::build(&args)?,
		deactivate: deactivate::Service::build(&args)?,
		oauth: oauth::Service::build(&args)?,
		retention: retention::Service::build(&args)?,
		registration_tokens: registration_tokens::Service::build(&args)?,
		replication: replication::Service::build(&args)?,

		manager: Mutex::new(None),
		server,
		db,
	});

	Ok(services.set(res))
}

#[implement(Services)]
pub(crate) fn services(&self) -> impl Iterator<Item = Arc<dyn Service>> + Send {
	macro_rules! cast {
		($s:expr) => {
			<Arc<dyn Service> as Into<_>>::into($s.clone())
		};
	}

	[
		cast!(self.account_data),
		cast!(self.admin),
		cast!(self.appservice),
		cast!(self.resolver),
		cast!(self.client),
		cast!(self.config),
		cast!(self.emergency),
		cast!(self.globals),
		cast!(self.key_backups),
		cast!(self.media),
		cast!(self.presence),
		cast!(self.pusher),
		cast!(self.alias),
		cast!(self.auth_chain),
		cast!(self.delete),
		cast!(self.directory),
		cast!(self.event_handler),
		cast!(self.lazy_loading),
		cast!(self.metadata),
		cast!(self.pdu_metadata),
		cast!(self.read_receipt),
		cast!(self.search),
		cast!(self.short),
		cast!(self.spaces),
		cast!(self.state),
		cast!(self.state_accessor),
		cast!(self.state_cache),
		cast!(self.state_compressor),
		cast!(self.threads),
		cast!(self.timeline),
		cast!(self.typing),
		cast!(self.federation),
		cast!(self.sending),
		cast!(self.server_keys),
		cast!(self.sync),
		cast!(self.transaction_ids),
		cast!(self.uiaa),
		cast!(self.users),
		cast!(self.membership),
		cast!(self.deactivate),
		cast!(self.oauth),
		cast!(self.retention),
		cast!(self.registration_tokens),
		cast!(self.replication),
	]
	.into_iter()
}

impl fmt::Debug for Services {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("Services").finish()
	}
}

#[implement(Services)]
pub async fn start(self: &Arc<Self>) -> Result<Arc<Self>> {
	debug_info!("Starting services...");

	super::migrations::migrations(self).await?;
	self.manager
		.lock()
		.await
		.insert(Manager::new(self))
		.clone()
		.start()
		.await?;

	debug_info!("Services startup complete.");

	Ok(Arc::clone(self))
}

#[implement(Services)]
pub async fn stop(&self) {
	info!("Shutting down services...");

	self.interrupt().await;
	if let Some(manager) = self.manager.lock().await.as_ref() {
		manager.stop().await;
	}

	// Everything that dispatches PDU counts has stopped, so the counter is
	// final and can be recorded for the next startup to read instead of
	// recovering it by scanning every PDU key. Deliberately last: if we never
	// reach here (crash, SIGKILL, power loss) no mark is written and the next
	// startup falls back to the scan, which is the safe direction.
	let count = self.globals.persist_clean_shutdown();
	info!(count, "Recorded clean shutdown counter.");

	// After the counter mark, which is itself a write: this records where the
	// database ends, and is only honoured if it still ends there at the next
	// open. Lets a primary that stopped cleanly prove it lost nothing when it
	// rejoins as a replica (see `tuwunel_database::rejoin`). Failing to write
	// it costs only that shortcut — the node does a full checkpoint instead.
	match self.db.record_clean_stop() {
		| Ok(()) => info!(seq = self.db.latest_wal_sequence(), "Recorded clean stop."),
		| Err(e) => warn!("Could not record clean stop; a rejoin will need a full checkpoint: {e}"),
	}

	debug_info!("Services shutdown complete.");
}

#[implement(Services)]
pub(crate) async fn interrupt(&self) {
	debug!("Interrupting services...");
	for service in self.services() {
		let name = service.name();
		trace!("Interrupting {name}");
		service.interrupt().await;
	}
}

#[implement(Services)]
pub async fn poll(&self) -> Result {
	if let Some(manager) = self.manager.lock().await.as_ref() {
		return manager.poll().await;
	}

	Ok(())
}

#[implement(Services)]
pub async fn clear_cache(&self) {
	self.services()
		.stream()
		.for_each(async |service| {
			service.clear_cache().await;
		})
		.await;
}

#[implement(Services)]
pub async fn memory_usage(&self) -> Result<String> {
	self.services()
		.try_stream()
		.try_fold(String::new(), async |mut out, service| {
			service.memory_usage(&mut out).await?;
			Ok(out)
		})
		.await
}

#[implement(Services)]
async fn maybe_bootstrap_checkpoint(server: &Arc<Server>) -> Result {
    use bytes::Bytes;

    let primary_url = match server.config.rocksdb_primary_url.as_ref() {
        Some(url) => url.clone(),
        None => return Ok(()),
    };

    let db_path = server.config.database_path.clone();
    let sidecar = db_path.parent()
        .unwrap_or(&db_path)
        .join("_replication_needs_bootstrap");

    let current_file = db_path.join("CURRENT");
    let needs_bootstrap = sidecar.exists() || !current_file.exists() || {
        std::fs::metadata(&current_file)
            .map(|m| m.len() == 0)
            .unwrap_or(true)
    };

    if !needs_bootstrap {
        return Ok(());
    }

    let token = server.config.rocksdb_replication_token.as_deref().unwrap_or("");

    info!("Pre-open bootstrap: downloading checkpoint from {primary_url}");

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        // No overall timeout: a full checkpoint of a large database takes many
        // minutes. A per-read timeout still ends a transfer that has stalled.
        .read_timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| tuwunel_core::err!(Database("Failed to build HTTP client: {e}")))?;

    let resp = client
        .get(format!("{primary_url}/_tuwunel/replication/checkpoint"))
        .header("x-tuwunel-replication-token", token)
        .send()
        .await
        .map_err(|e| tuwunel_core::err!(Database("Checkpoint request failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(tuwunel_core::err!(Database(
            "Primary returned {} for checkpoint", resp.status()
        )));
    }

    // Absent when the primary runs a build without fast rejoin.
    let primary_instance: Option<String> = resp.headers()
        .get("x-tuwunel-instance-id")
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned);

    // The resume cursor for this checkpoint. Without it there is no knowing
    // where the checkpoint ends; guessing 0 would replay whatever WAL the
    // primary still has onto it, hole and all.
    let seq: u64 = resp.headers()
        .get("x-tuwunel-checkpoint-sequence")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| tuwunel_core::err!(Database(
            "Primary sent a checkpoint without a readable x-tuwunel-checkpoint-sequence"
        )))?;

    let parent = db_path.parent().unwrap_or(&db_path);
    let staging = parent.join("_replication_staging");
    let backup = parent.join("_replication_backup");

    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .map_err(|e| tuwunel_core::err!(Database("Removing staging dir: {e}")))?;
    }
    std::fs::create_dir_all(&staging)
        .map_err(|e| tuwunel_core::err!(Database("Creating staging dir: {e}")))?;

    // Unpack while downloading. The body used to be read whole with
    // `resp.bytes()` and unpacked from memory, which needs RAM the size of the
    // database (tens of GB) on the secondary. Now chunks go straight from the
    // socket to a blocking tar reader through a bounded channel, so at most
    // BOOTSTRAP_CHANNEL_DEPTH chunks are held at once.
    const BOOTSTRAP_CHANNEL_DEPTH: usize = 16;
    let (chunk_tx, chunk_rx) =
        tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(BOOTSTRAP_CHANNEL_DEPTH);

    let unpack_dir = staging.clone();
    let unpack = tokio::task::spawn_blocking(move || {
        let reader = ChannelReader { rx: chunk_rx, current: Bytes::new() };
        tar::Archive::new(reader).unpack(&unpack_dir)
    });

    let mut resp = resp;
    let mut received: u64 = 0;
    let mut next_report: u64 = BOOTSTRAP_PROGRESS_BYTES;
    let download: std::result::Result<(), String> = loop {
        match resp.chunk().await {
            | Ok(Some(chunk)) => {
                received = received.saturating_add(chunk.len() as u64);
                if received >= next_report {
                    info!("Pre-open bootstrap: {} MiB received", received / (1024 * 1024));
                    next_report = next_report.saturating_add(BOOTSTRAP_PROGRESS_BYTES);
                }
                if chunk_tx.send(Ok(chunk)).await.is_err() {
                    // The unpacker stopped early; its own error says why.
                    break Ok(());
                }
            },
            | Ok(None) => break Ok(()),
            | Err(e) => {
                let msg = format!("Reading checkpoint body: {e}");
                let _ = chunk_tx
                    .send(Err(std::io::Error::other(msg.clone())))
                    .await;
                break Err(msg);
            },
        }
    };
    drop(chunk_tx);

    let unpacked = unpack.await
        .map_err(|e| tuwunel_core::err!(Database("Checkpoint unpack task failed: {e}")))?;
    download.map_err(|e| tuwunel_core::err!(Database("{e}")))?;
    unpacked.map_err(|e| tuwunel_core::err!(Database("Unpacking checkpoint: {e}")))?;

    info!("Pre-open bootstrap: checkpoint received ({} MiB)", received / (1024 * 1024));

    let checkpoint_src = staging.join("checkpoint");

    if backup.exists() {
        std::fs::remove_dir_all(&backup)
            .map_err(|e| tuwunel_core::err!(Database("Removing backup: {e}")))?;
    }
    if db_path.exists() {
        std::fs::rename(&db_path, &backup)
            .map_err(|e| tuwunel_core::err!(Database("Moving db to backup: {e}")))?;
    }
    std::fs::rename(&checkpoint_src, &db_path)
        .map_err(|e| tuwunel_core::err!(Database("Moving checkpoint to db_path: {e}")))?;

    let _: std::io::Result<()> = std::fs::remove_dir_all(&staging);
    // The restored directory is, right now, an exact copy of the primary's
    // database. Say so; the marker is dropped at this node's first local data
    // write (see `tuwunel_database::rejoin`). Not being able to write it only
    // costs the fast-rejoin shortcut later.
    if let Some(primary_instance) = primary_instance.as_deref()
        && let Err(e) = tuwunel_database::rejoin::mark_restored_checkpoint(&db_path, primary_instance)
    {
        warn!("Could not mark the restored checkpoint as an exact copy: {e}");
    }
    // Cursor, then the primary's instance id if it gave one. The worker records
    // both, so that a primary which comes back as a different database before
    // this node's first connection is still recognised as one.
    let sidecar_body = match primary_instance.as_deref() {
        Some(instance) => format!("{seq} {instance}"),
        None => seq.to_string(),
    };
    std::fs::write(&sidecar, sidecar_body)
        .map_err(|e| tuwunel_core::err!(Database("Writing bootstrap sidecar: {e}")))?;

    info!("Pre-open bootstrap complete; resume_seq = {seq}. RocksDB will open clean checkpoint.");
    Ok(())
}

/// Log a progress line every this many bytes of checkpoint download.
const BOOTSTRAP_PROGRESS_BYTES: u64 = 1024 * 1024 * 1024;

/// Blocking `Read` over checkpoint chunks arriving from the async download.
struct ChannelReader {
    rx: tokio::sync::mpsc::Receiver<std::io::Result<bytes::Bytes>>,
    current: bytes::Bytes,
}

impl std::io::Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.current.is_empty() {
            match self.rx.blocking_recv() {
                | Some(Ok(chunk)) => self.current = chunk,
                | Some(Err(e)) => return Err(e),
                | None => return Ok(0), // sender dropped: end of body
            }
        }

        let n = buf.len().min(self.current.len());
        buf[..n].copy_from_slice(&self.current.split_to(n));
        Ok(n)
    }
}

#[cfg(test)]
mod bootstrap_stream_tests {
    use super::ChannelReader;

    fn archive_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn out_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tuwunel-bootstrap-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Chunks of awkward sizes (splitting tar headers mid-block) reassemble
    /// into the same files.
    #[test]
    fn unpacks_archive_fed_in_odd_chunks() {
        let big: Vec<u8> = (0..200_000u32).map(|i| (i % 253) as u8).collect();
        let archive = archive_of(&[("checkpoint/000001.sst", &big), ("checkpoint/CURRENT", b"M\n")]);

        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let dir = out_dir("ok");
        let unpack_dir = dir.clone();
        let unpack = std::thread::spawn(move || {
            tar::Archive::new(ChannelReader { rx, current: bytes::Bytes::new() }).unpack(&unpack_dir)
        });
        for piece in archive.chunks(777) {
            tx.blocking_send(Ok(bytes::Bytes::copy_from_slice(piece))).unwrap();
        }
        drop(tx);

        unpack.join().unwrap().unwrap();
        assert_eq!(std::fs::read(dir.join("checkpoint/000001.sst")).unwrap(), big);
        assert_eq!(std::fs::read(dir.join("checkpoint/CURRENT")).unwrap(), b"M\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A download error mid-stream fails the unpack; it must never look like a
    /// complete (truncated) checkpoint.
    #[test]
    fn download_error_fails_unpack() {
        let big = vec![7u8; 100_000];
        let archive = archive_of(&[("checkpoint/000001.sst", &big)]);

        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let dir = out_dir("err");
        let unpack_dir = dir.clone();
        let unpack = std::thread::spawn(move || {
            tar::Archive::new(ChannelReader { rx, current: bytes::Bytes::new() }).unpack(&unpack_dir)
        });
        tx.blocking_send(Ok(bytes::Bytes::copy_from_slice(&archive[..archive.len() / 2])))
            .unwrap();
        tx.blocking_send(Err(std::io::Error::other("connection reset"))).unwrap();
        drop(tx);

        assert!(unpack.join().unwrap().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
