use std::{collections::BTreeMap, sync::Arc};

use tuwunel_core::Result;

use crate::{
	Engine, Map,
	engine::descriptor::{self, CacheDisp, Descriptor},
};

pub(super) type Maps = BTreeMap<MapsKey, MapsVal>;
pub(super) type MapsKey = &'static str;
pub(super) type MapsVal = Arc<Map>;

pub(super) fn open(engine: &Arc<Engine>) -> Result<Maps> { open_list(engine, MAPS) }

#[tracing::instrument(name = "maps", level = "debug", skip_all)]
pub(super) fn open_list(engine: &Arc<Engine>, maps: &[Descriptor]) -> Result<Maps> {
	maps.iter()
		.filter(|desc| !desc.dropped)
		.map(|desc| Ok((desc.name, Map::open(engine, desc.name)?)))
		.collect()
}

pub(super) static MAPS: &[Descriptor] = &[
	Descriptor {
		name: "alias_roomid",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "alias_userid",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "aliasid_alias",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "authchainkey_authchain",
		cache_disp: CacheDisp::SharedWith("shorteventid_authchain"),
		index_size: 512,
		block_size: 4096,
		key_size_hint: Some(8), // intentionally match shorteventid_authchain
		val_size_hint: Some(256),
		..descriptor::RANDOM_CACHE
	},
	Descriptor {
		name: "backupid_algorithm",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "backupid_etag",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "backupkeyid_backup",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "bannedroomids",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "disabledroomids",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "eventid_originalpdu",
		key_size_hint: Some(48),
		val_size_hint: Some(1520),
		block_size: 2048,
		index_size: 512,
		..descriptor::RANDOM
	},
	Descriptor {
		name: "eventid_outlierpdu",
		cache_disp: CacheDisp::SharedWith("pduid_pdu"),
		key_size_hint: Some(48),
		val_size_hint: Some(1488),
		block_size: 1024,
		index_size: 512,
		..descriptor::RANDOM
	},
	Descriptor {
		name: "eventid_pduid",
		cache_disp: CacheDisp::Unique,
		key_size_hint: Some(48),
		val_size_hint: Some(16),
		block_size: 512,
		index_size: 512,
		..descriptor::RANDOM
	},
	Descriptor {
		name: "eventid_shorteventid",
		cache_disp: CacheDisp::Unique,
		key_size_hint: Some(48),
		val_size_hint: Some(8),
		block_size: 512,
		index_size: 512,
		..descriptor::RANDOM
	},
	Descriptor {
		name: "global",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "id_appserviceregistrations",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "keychangeid_userid",
		..descriptor::RANDOM
	},
	Descriptor {
		name: "keyid_key",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "lazyloadedids",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "logintoken_expiresatuserid",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "mediaid_file",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "mediaid_user",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "oauthid_session",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "oauthuniqid_oauthid",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "onetimekeyid_onetimekeys",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "openidtoken_expiresatuserid",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "pduid_pdu",
		cache_disp: CacheDisp::SharedWith("eventid_outlierpdu"),
		key_size_hint: Some(16),
		val_size_hint: Some(1520),
		block_size: 2048,
		index_size: 512,
		..descriptor::SEQUENTIAL
	},
	Descriptor {
		name: "publicroomids",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "pushkey_deviceid",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "presenceid_presence",
		..descriptor::SEQUENTIAL_SMALL
	},
	Descriptor {
		name: "readreceiptid_readreceipt",
		..descriptor::RANDOM
	},
	Descriptor {
		name: "referencedevents",
		..descriptor::RANDOM
	},
	Descriptor {
		name: "registrationtoken_info",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "roomid_knockedcount",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "roomid_invitedcount",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "roomid_inviteviaservers",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "roomid_joinedcount",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "roomid_maxremotepowerlevel",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "roomid_pduleaves",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "roomid_shortroomid",
		val_size_hint: Some(8),
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "roomid_shortstatehash",
		val_size_hint: Some(8),
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "roomserverids",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "roomsynctoken_shortstatehash",
		..descriptor::DROPPED
	},
	Descriptor {
		name: "roomuserdataid_accountdata",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "roomuserid_invitecount",
		val_size_hint: Some(8),
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "roomuserid_joined",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "roomuserid_lastprivatereadupdate",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "roomuserid_lastnotificationread",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "roomuserid_leftcount",
		val_size_hint: Some(8),
		..descriptor::RANDOM
	},
	Descriptor {
		name: "roomuserid_knockedcount",
		val_size_hint: Some(8),
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "roomuserid_privateread",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "roomuseroncejoinedids",
		..descriptor::RANDOM
	},
	Descriptor {
		name: "roomusertype_roomuserdataid",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "senderkey_pusher",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "server_signingkeys",
		..descriptor::RANDOM
	},
	Descriptor {
		name: "servercurrentevent_data",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "servername_destination",
		..descriptor::RANDOM_SMALL_CACHE
	},
	Descriptor {
		name: "servername_educount",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "servername_override",
		..descriptor::RANDOM_SMALL_CACHE
	},
	Descriptor {
		name: "servernameevent_data",
		cache_disp: CacheDisp::Unique,
		val_size_hint: Some(128),
		..descriptor::RANDOM
	},
	Descriptor {
		name: "serverroomids",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "shorteventid_authchain",
		cache_disp: CacheDisp::SharedWith("authchainkey_authchain"),
		key_size_hint: Some(8),
		val_size_hint: Some(256),
		index_size: 512,
		block_size: 4096,
		..descriptor::SEQUENTIAL
	},
	Descriptor {
		name: "shorteventid_eventid",
		cache_disp: CacheDisp::Unique,
		key_size_hint: Some(8),
		val_size_hint: Some(48),
		..descriptor::SEQUENTIAL_SMALL
	},
	Descriptor {
		name: "shorteventid_shortstatehash",
		key_size_hint: Some(8),
		val_size_hint: Some(8),
		block_size: 512,
		index_size: 512,
		..descriptor::SEQUENTIAL
	},
	Descriptor {
		name: "shortstatehash_statediff",
		key_size_hint: Some(8),
		..descriptor::SEQUENTIAL_SMALL
	},
	Descriptor {
		name: "shortstatekey_statekey",
		cache_disp: CacheDisp::Unique,
		key_size_hint: Some(8),
		val_size_hint: Some(1016),
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "softfailedeventids",
		key_size_hint: Some(48),
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "statehash_shortstatehash",
		val_size_hint: Some(8),
		..descriptor::RANDOM
	},
	Descriptor {
		name: "statekey_shortstatekey",
		cache_disp: CacheDisp::Unique,
		key_size_hint: Some(1016),
		val_size_hint: Some(8),
		..descriptor::RANDOM
	},
	Descriptor {
		name: "threadid_userids",
		..descriptor::SEQUENTIAL_SMALL
	},
	Descriptor {
		name: "timeredacted_eventid",
		key_size_hint: Some(57),
		..descriptor::SEQUENTIAL_SMALL
	},
	Descriptor {
		name: "todeviceid_events",
		..descriptor::RANDOM
	},
	Descriptor {
		name: "tofrom_relation",
		key_size_hint: Some(8),
		val_size_hint: Some(8),
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "token_userdeviceid",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "tokenids",
		block_size: 512,
		..descriptor::RANDOM
	},
	Descriptor {
		name: "url_previews",
		..descriptor::RANDOM
	},
	Descriptor {
		name: "userdeviceid_metadata",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "userdeviceconnid_conn",
		block_size: 1024 * 16,
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "userdeviceid_refresh",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "userdeviceid_token",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "userdevicesessionid_uiaainfo",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "userdevicetxnid_response",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "userfilterid_filter",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "userid_avatarurl",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "userid_blurhash",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "userid_dehydrateddevice",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "userid_devicelistversion",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "userid_displayname",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "userid_lastonetimekeyupdate",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "userid_masterkeyid",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "userid_oauthid",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "userid_origin",
		..descriptor::RANDOM
	},
	Descriptor {
		name: "userid_password",
		..descriptor::RANDOM
	},
	Descriptor {
		name: "userid_presenceid",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "userid_selfsigningkeyid",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "userid_usersigningkeyid",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "useridcount_notification",
		limit_size: 1024 * 1024 * 256,
		..descriptor::RANDOM_SMALL_CACHE
	},
	Descriptor {
		name: "useridprofilekey_value",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "userroomid_highlightcount",
		..descriptor::RANDOM
	},
	Descriptor {
		name: "userroomid_invitestate",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "userroomid_joined",
		..descriptor::RANDOM
	},
	Descriptor {
		name: "userroomid_leftstate",
		..descriptor::RANDOM
	},
	Descriptor {
		name: "userroomid_knockedstate",
		..descriptor::RANDOM_SMALL
	},
	Descriptor {
		name: "userroomid_notificationcount",
		..descriptor::RANDOM
	},
];
