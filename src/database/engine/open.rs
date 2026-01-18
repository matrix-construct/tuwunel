use std::{
	collections::BTreeSet,
	path::Path,
	sync::{Arc, atomic::AtomicU32},
};

use rocksdb::{ColumnFamilyDescriptor, Options};
use tuwunel_core::{
	Result, debug, debug_warn, implement, info, itertools::Itertools, trace, warn,
};

use super::{
	Db, Engine, cf_opts::cf_options, context, db_opts::db_options, descriptor,
	descriptor::Descriptor, repair::repair,
};
use crate::{Context, or_else};

#[implement(Engine)]
#[tracing::instrument(skip_all)]
pub(crate) async fn open(ctx: Arc<Context>, desc: &[Descriptor]) -> Result<Arc<Self>> {
	let server = &ctx.server;
	let config = &server.config;
	let path = &config.database_path;

	context::before_open(&ctx, path)?;
	let db_opts = db_options(
		config,
		&ctx.env.lock().expect("environment locked"),
		&ctx.row_cache.lock().expect("row cache locked"),
	)?;

	let (cfds, dropped) = Self::configure_cfds(&ctx, &db_opts, desc)?;
	let num_cfds = cfds.len();
	debug!("Configured {num_cfds} column descriptors...");

	let load_time = std::time::Instant::now();
	if config.rocksdb_repair {
		repair(&db_opts, &config.database_path)?;
	}

	debug!("Opening database...");
	let db = if config.rocksdb_read_only {
		Db::open_cf_descriptors_read_only(&db_opts, path, cfds, false)
	} else if config.rocksdb_secondary {
		Db::open_cf_descriptors_as_secondary(&db_opts, path, path, cfds)
	} else {
		Db::open_cf_descriptors(&db_opts, path, cfds)
	}
	.or_else(or_else)?;

	if !config.rocksdb_read_only && !config.rocksdb_secondary {
		for name in &dropped {
			debug!("Deleting dropped column {name:?} ...");
			db.drop_cf(name).or_else(or_else)?;
		}
	}

	info!(
		columns = num_cfds,
		sequence = %db.latest_sequence_number(),
		time = ?load_time.elapsed(),
		"Opened database."
	);

	Ok(Arc::new(Self {
		db,
		pool: ctx.pool.clone(),
		ctx: ctx.clone(),
		read_only: config.rocksdb_read_only,
		secondary: config.rocksdb_secondary,
		checksums: config.rocksdb_checksums,
		corks: AtomicU32::new(0),
	}))
}

#[implement(Engine)]
#[tracing::instrument(name = "configure", skip_all)]
fn configure_cfds(
	ctx: &Arc<Context>,
	db_opts: &Options,
	desc: &[Descriptor],
) -> Result<(Vec<ColumnFamilyDescriptor>, Vec<String>)> {
	let server = &ctx.server;
	let config = &server.config;
	let path = &config.database_path;
	let existing = Self::discover_cfs(path, db_opts);

	// Found columns which are not described.
	let missing = existing
		.iter()
		.map(String::as_str)
		.filter(|&name| name != "default")
		.filter(|&name| !desc.iter().any(|desc| desc.name == name));

	// Described columns which are not found.
	let creating = desc
		.iter()
		.filter(|desc| !desc.dropped)
		.filter(|desc| !existing.contains(desc.name));

	// Found columns which are described as dropped.
	let dropping = desc
		.iter()
		.filter(|desc| desc.dropped)
		.filter(|desc| existing.contains(desc.name))
		.filter(|_| !config.rocksdb_never_drop_columns);

	// Described dropped columns which are no longer found.
	let dropped = desc
		.iter()
		.filter(|desc| desc.dropped)
		.filter(|desc| !existing.contains(desc.name));

	debug!(
		existing = existing.len(),
		described = desc.len(),
		missing = missing.clone().count(),
		dropped = dropped.clone().count(),
		creating = creating.clone().count(),
		dropping = dropping.clone().count(),
		"Discovered database columns"
	);

	missing.clone().for_each(|name| {
		debug_warn!("Found undescribed column {name:?} in existing database.");
	});

	dropped
		.clone()
		.map(|desc| desc.name)
		.for_each(|name| {
			debug!("Previously dropped column {name:?} no longer found in database.");
		});

	creating
		.clone()
		.map(|desc| desc.name)
		.for_each(|name| {
			debug!("Creating new column {name:?} not previously found in existing database.");
		});

	dropping
		.clone()
		.map(|desc| desc.name)
		.for_each(|name| {
			warn!(
				"Column {name:?} has been scheduled for deletion. Storage may not appear \
				 reclaimed until further restart or compaction."
			);
		});

	let not_dropped = |desc: &&Descriptor| {
		!dropped
			.clone()
			.any(|dropped| desc.name == dropped.name)
	};

	let dropping = dropping
		.map(|desc| desc.name)
		.map(ToOwned::to_owned)
		.collect();

	let cfnames = desc
		.iter()
		.filter(not_dropped)
		.map(|desc| desc.name)
		.chain(missing.clone());

	let cfds: Vec<_> = desc
		.iter()
		.filter(not_dropped)
		.copied()
		.chain(missing.map(|_| descriptor::IGNORED))
		.zip(cfnames)
		.inspect(|&(desc, name)| {
			assert!(
				desc.ignored || desc.name == name,
				"{name:?} does not match descriptor {:?}",
				desc.name
			);
		})
		.inspect(|&(_, name)| debug!(name, "Described column"))
		.map(|(desc, name)| (desc, name.to_owned()))
		.map(|(desc, name)| Ok((name, cf_options(ctx, db_opts.clone(), &desc)?)))
		.map_ok(|(name, opts)| ColumnFamilyDescriptor::new(name, opts))
		.collect::<Result<_>>()?;

	trace!(?dropping);
	Ok((cfds, dropping))
}

#[implement(Engine)]
#[tracing::instrument(name = "discover", skip_all)]
fn discover_cfs(path: &Path, opts: &Options) -> BTreeSet<String> {
	Db::list_cf(opts, path)
		.unwrap_or_default()
		.into_iter()
		.collect::<BTreeSet<_>>()
}
