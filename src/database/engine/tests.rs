use std::{env::temp_dir, fs, path::Path, process::id, sync::Arc, thread::spawn};

use rocksdb::{
	Cache, DB, Env as RocksEnv, Options,
	backup::{BackupEngine, BackupEngineOptions, RestoreOptions},
};

use super::{
	cf_opts::{register_pool, set_preview_retention},
	context::{ColCache, ColCaches, SHARED_POOL},
	descriptor::{self, CacheDisp, Descriptor},
	env::Env,
	open::is_remnant,
};
use crate::maps::descriptor;

fn fresh_caches() -> ColCaches {
	let shared = ColCache {
		cache: Cache::new_lru_cache(1024),
		participants: Vec::new(),
	};

	[(SHARED_POOL, shared)].into()
}

#[test]
fn unique_lands_in_its_own_pool() {
	let mut caches = fresh_caches();
	let desc = Descriptor {
		name: "unique_a",
		cache_disp: CacheDisp::Unique,
		..descriptor::RANDOM
	};

	let cache = register_pool(&mut caches, &desc, || Cache::new_lru_cache(1024));
	assert!(cache.is_some());

	let pool = caches
		.get("unique_a")
		.expect("unique pool registered under own name");

	assert_eq!(pool.participants, vec!["unique_a"]);
}

#[test]
fn unique_zero_capacity_returns_none() {
	let mut caches = fresh_caches();
	let desc = Descriptor {
		name: "unique_empty",
		cache_disp: CacheDisp::Unique,
		cache_size: 0,
		..descriptor::RANDOM
	};

	let cache = register_pool(&mut caches, &desc, || Cache::new_lru_cache(1024));
	assert!(cache.is_none());
	assert!(!caches.contains_key("unique_empty"), "zero-cap unique must not register");
}

#[test]
fn shared_pair_collapses_to_one_pool_with_both_participants() {
	let mut caches = fresh_caches();
	let first = Descriptor {
		name: "first_arrival",
		cache_disp: CacheDisp::SharedWith("second_arrival"),
		..descriptor::RANDOM
	};
	let second = Descriptor {
		name: "second_arrival",
		cache_disp: CacheDisp::SharedWith("first_arrival"),
		..descriptor::RANDOM
	};

	let _cache_a = register_pool(&mut caches, &first, || Cache::new_lru_cache(1024))
		.expect("first arrival builds cache");

	let _cache_b = register_pool(&mut caches, &second, || {
		panic!("second arrival must not rebuild cache");
	})
	.expect("second arrival receives same cache");

	let pool = caches
		.get("first_arrival")
		.expect("pool registered under first arrival");

	assert_eq!(pool.participants, vec!["first_arrival", "second_arrival"]);
	assert!(!caches.contains_key("second_arrival"));
}

#[test]
fn shared_disposition_joins_global_pool() {
	let mut caches = fresh_caches();
	let desc_one = Descriptor {
		name: "shared_one",
		cache_disp: CacheDisp::Shared,
		..descriptor::RANDOM
	};
	let desc_two = Descriptor {
		name: "shared_two",
		cache_disp: CacheDisp::Shared,
		..descriptor::RANDOM
	};

	register_pool(&mut caches, &desc_one, || panic!("Shared must reuse, not build"));
	register_pool(&mut caches, &desc_two, || panic!("Shared must reuse, not build"));

	let pool = caches
		.get(SHARED_POOL)
		.expect("shared pool present");

	assert_eq!(pool.participants, vec!["shared_one", "shared_two"]);
}

#[test]
fn restore_selects_backup_and_preserves_media_dir() {
	let root = temp_dir().join(format!("tuwunel-restore-test-{}", id()));
	let db_dir = root.join("db");
	let backup_dir = root.join("backup");
	fs::create_dir_all(&db_dir).expect("create db dir");
	fs::create_dir_all(&backup_dir).expect("create backup dir");

	let env = Env::acquire_with_priorities(false, false).expect("acquire environment");
	let other = Env::acquire_with_priorities(false, false).expect("acquire another owner");

	assert!(Arc::ptr_eq(&env, &other));

	let backup_opts = BackupEngineOptions::new(&backup_dir).expect("create backup options");
	let opts = options(&env.lock().expect("environment locked"));
	let engine = BackupEngine::open(&backup_opts, &env.lock().expect("environment locked"))
		.expect("open backup engine");

	let db = DB::open(&opts, &db_dir).expect("open fresh db");
	let owner = Arc::downgrade(&other);

	spawn(move || drop(other))
		.join()
		.expect("drop overlapping environment owner");

	assert!(
		owner
			.upgrade()
			.is_some_and(|owner| Arc::ptr_eq(&env, &owner))
	);

	db.put(b"first", b"before backup")
		.expect("put first");

	let engine = create_backup(engine, &db);

	db.put(b"second", b"after backup #1")
		.expect("put second");

	let engine = create_backup(engine, &db);

	drop(db);

	let ids: Vec<_> = engine
		.get_backup_info()
		.iter()
		.map(|info| info.backup_id)
		.collect();

	assert_eq!(ids, [1, 2], "backup ids ascend from 1, reserving 0 for most-recent");

	let media = db_dir.join("media");
	fs::create_dir_all(&media).expect("create media dir");
	fs::write(media.join("marker"), b"media file").expect("write media marker");

	let engine = restore_backup(engine, &db_dir);

	let marker = fs::read(media.join("marker")).expect("media dir survives restore");

	assert_eq!(marker, b"media file");

	let db = DB::open(&opts, &db_dir).expect("open restored db");

	assert_eq!(
		db.get(b"first").expect("get first").as_deref(),
		Some(&b"before backup"[..]),
		"restored db holds the backed-up value"
	);
	assert_eq!(
		db.get(b"second").expect("get second"),
		None,
		"value written after backup #1 is rolled back"
	);

	drop(db);
	drop(engine);
	drop(opts);
	drop(env);
	fs::remove_dir_all(&root).ok();
}

fn options(env: &RocksEnv) -> Options {
	let mut opts = Options::default();

	opts.create_if_missing(true);
	opts.set_env(env);

	opts
}

fn create_backup(mut engine: BackupEngine, db: &DB) -> BackupEngine {
	engine
		.create_new_backup_flush(db, true)
		.expect("create backup");

	engine
}

fn restore_backup(mut engine: BackupEngine, dir: &Path) -> BackupEngine {
	engine
		.restore_from_backup(dir, dir, &RestoreOptions::default(), 1)
		.expect("restore backup #1");

	engine
}

#[test]
fn remnants_classified_by_name() {
	assert!(is_remnant("CURRENT"));
	assert!(is_remnant("MANIFEST-000005"));
	assert!(is_remnant("000004.log"));
	assert!(is_remnant("000123.sst"));

	assert!(!is_remnant("LOCK"));
	assert!(!is_remnant("IDENTITY"));
	assert!(!is_remnant("OPTIONS-000007"));
	assert!(!is_remnant("media"));
	assert!(!is_remnant("conduit.db"));
	assert!(!is_remnant(".sst"));
	assert!(!is_remnant("backup.log"));
}

#[test]
fn preview_columns_cover_configured_lifetime() {
	for ttl in [0, 60 * 60 * 24, 60 * 60 * 24 * 365] {
		let mut preview = *descriptor("url_preview");
		let mut lazy_media = *descriptor("mediaid_lazy");

		set_preview_retention(&mut preview, ttl);
		set_preview_retention(&mut lazy_media, ttl);

		assert!(preview.ttl >= ttl, "url_preview ttl below the configured lifetime");
		assert!(lazy_media.ttl >= preview.ttl, "mediaid_lazy ttl below url_preview ttl");
	}
}
