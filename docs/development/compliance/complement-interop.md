# Tuwunel Complement Interoperability Test Results

Tuwunel runs Complement with Tuwunel and Synapse assigned to opposite homeserver positions. In the first direction Tuwunel is `hs1` and Synapse is `hs2`; the inverse direction swaps those positions.

These report-only results are a snapshot from [July 24, 2026 CI run](https://github.com/matrix-construct/tuwunel/actions/runs/30074568131).

## Counts

### Tuwunel as `hs1`

- Test groups: 205. Passing: **81.5%**
  - ✅ `pass`: 167
  - 🟨 `some`: 18
  - ❌ `fail`: 18
  - ⬛ `skip`: 2

- Subtests: 587. Passing: **77.0%**
  - ✅ `pass`: 452
  - ❌ `fail`: 122
  - ⬛ `skip`: 13

### Synapse as `hs1`

- Test groups: 205. Passing: **92.7%**
  - ✅ `pass`: 190
  - 🟨 `some`: 9
  - ❌ `fail`: 6
  - ⬛ `skip`: 0

- Subtests: 612. Passing: **94.6%**
  - ✅ `pass`: 579
  - ❌ `fail`: 25
  - ⬛ `skip`: 8

## All Top-Level Tests

| Tuwunel as `hs1` | Subtests | Synapse as `hs1` | Subtests | Test |
|---|---|---|---|---|
| ✅ | – | ✅ | – | `ACLs` |
| ✅ | – | ✅ | – | `ACLsForEDUs` |
| ✅ | 2/0/0 | ✅ | 2/0/0 | `AddAccountData` |
| 🟨 | 3/2/1 | ✅ | 5/0/1 | `ArchivedRoomsHistory` |
| ✅ | 6/0/0 | ✅ | 6/0/0 | `AsyncUpload` |
| ✅ | – | ✅ | – | `AvatarUrlUpdate` |
| ✅ | – | ✅ | – | `BannedUserCannotSendJoin` |
| ⬛ | – | ✅ | – | `CanRegisterAdmin` |
| ✅ | – | ✅ | – | `CannotKickLeftUser` |
| ✅ | – | ✅ | – | `CannotKickNonPresentUser` |
| ✅ | 6/0/0 | ✅ | 6/0/0 | `CannotSendKnockViaSendKnockInMSC3787Room` |
| ✅ | 6/0/0 | ✅ | 6/0/0 | `CannotSendNonJoinViaSendJoinV2` |
| ✅ | 6/0/0 | ✅ | 6/0/0 | `CannotSendNonKnockViaSendKnock` |
| ✅ | 6/0/0 | ✅ | 6/0/0 | `CannotSendNonLeaveViaSendLeaveV2` |
| ✅ | 5/0/0 | ✅ | 5/0/0 | `ChangePassword` |
| ✅ | 2/0/0 | ✅ | 2/0/0 | `ChangePasswordPushers` |
| 🟨 | 1/4/0 | ✅ | 5/0/0 | `ClientSpacesSummary` |
| ✅ | – | ✅ | – | `ClientSpacesSummaryJoinRules` |
| ✅ | – | ✅ | – | `ComplementCanCreateValidV12Rooms` |
| ✅ | – | ❌ | – | `Content` |
| ✅ | – | ✅ | – | `ContentCSAPIMediaV1` |
| ✅ | – | ✅ | – | `ContentMediaV1` |
| ❌ | – | ✅ | – | `CorruptedAuthChain` |
| ✅ | – | ✅ | – | `CumulativeJoinLeaveJoinSync` |
| ✅ | 4/0/0 | ✅ | 4/0/0 | `DeactivateAccount` |
| 🟨 | 6/7/1 | ✅ | 17/0/1 | `DelayedEvents` |
| ✅ | 1/0/0 | ✅ | 1/0/0 | `DeletingDeviceRemovesDeviceLocalNotificationSettings` |
| ✅ | – | ✅ | – | `DemotingUsersViaUsersDefault` |
| 🟨 | 8/2/0 | 🟨 | 9/1/0 | `DeviceListUpdates` |
| ✅ | 2/0/0 | ✅ | 2/0/0 | `DeviceListsUpdateOverFederation` |
| ❌ | – | ❌ | – | `DeviceListsUpdateOverFederationOnRoomJoin` |
| ✅ | 7/0/0 | ✅ | 7/0/0 | `DeviceManagement` |
| ✅ | – | ✅ | – | `DisplayNameUpdate` |
| ✅ | 3/0/0 | ✅ | 3/0/0 | `E2EKeyBackupReplaceRoomKeyRules` |
| ✅ | 3/0/0 | ✅ | 3/0/0 | `Event` |
| ✅ | 2/0/0 | ✅ | 2/0/0 | `EventAuth` |
| ❌ | – | ❌ | – | `EventRelationships` |
| ✅ | – | ✅ | – | `FederatedClientSpaces` |
| ❌ | – | ❌ | – | `FederatedEventRelationships` |
| 🟨 | 1/1/0 | ✅ | 2/0/0 | `FederationKeyUploadQuery` |
| ✅ | – | ✅ | – | `FederationRedactSendsWithoutEvent` |
| ✅ | – | ✅ | – | `FederationRejectInvite` |
| ✅ | 10/0/0 | ✅ | 10/0/0 | `FederationRoomsInvite` |
| ✅ | 2/0/0 | ✅ | 2/0/0 | `FederationSlidingSyncReInviteAfterLeave` |
| ✅ | – | ✅ | – | `FederationThumbnail` |
| ✅ | – | ✅ | – | `FetchEvent` |
| ✅ | – | ✅ | – | `FetchEventNonWorldReadable` |
| ✅ | – | ✅ | – | `FetchEventWorldReadable` |
| ✅ | – | ✅ | – | `FetchHistoricalInvitedEventFromBeforeInvite` |
| ✅ | – | ✅ | – | `FetchHistoricalInvitedEventFromBetweenInvite` |
| ✅ | – | ✅ | – | `FetchHistoricalJoinedEventDenied` |
| ✅ | – | ✅ | – | `FetchHistoricalSharedEvent` |
| ✅ | – | ✅ | – | `FetchMessagesFromNonExistentRoom` |
| ✅ | – | ✅ | – | `Filter` |
| ❌ | – | ✅ | – | `FilterMessagesByRelType` |
| ✅ | – | ✅ | – | `GappedSyncLeaveSection` |
| ✅ | 3/0/0 | ✅ | 3/0/0 | `GetFilteredRoomMembers` |
| ✅ | – | ✅ | – | `GetMissingEventsGapFilling` |
| ✅ | – | ✅ | – | `GetRoomMembers` |
| ❌ | – | ✅ | – | `GetRoomMembersAtPoint` |
| ❌ | 0/4/0 | ✅ | 4/0/0 | `InboundCanReturnMissingEvents` |
| ✅ | – | ✅ | – | `InboundFederationKeys` |
| ✅ | 2/0/0 | ✅ | 2/0/0 | `InboundFederationProfile` |
| ❌ | – | ✅ | 3/0/0 | `InboundFederationRejectsEventsWithRejectedAuthEvents` |
| 🟨 | 3/8/0 | ✅ | 11/0/0 | `InviteFiltering` |
| ✅ | – | ✅ | – | `InviteFromIgnoredUsersDoesNotAppearInSync` |
| ✅ | – | ✅ | – | `IsDirectFlagFederation` |
| ✅ | – | ✅ | – | `IsDirectFlagLocal` |
| ✅ | – | ✅ | – | `JoinFederatedRoomFailOver` |
| ✅ | 1/0/0 | ✅ | 1/0/0 | `JoinFederatedRoomFromApplicationServiceBridgeUser` |
| ✅ | 4/0/0 | ✅ | 4/0/0 | `JoinFederatedRoomWithUnverifiableEvents` |
| ✅ | – | ✅ | – | `JoinViaRoomIDAndServerName` |
| ✅ | 3/0/0 | ✅ | 3/0/0 | `Json` |
| ✅ | 7/0/0 | ✅ | 7/0/0 | `JumpToDateEndpoint` |
| ✅ | 1/0/0 | ✅ | 1/0/0 | `KeyChangesLocal` |
| ✅ | – | ✅ | – | `KeyClaimOrdering` |
| ✅ | – | ✅ | – | `KeysQueryWithDeviceIDAsObjectFails` |
| ✅ | – | ✅ | – | `KnockRestrictedRoomsLocalJoinNoCreatorsUsesPowerLevelsV11` |
| ✅ | – | ✅ | – | `KnockRestrictedRoomsLocalJoinNoCreatorsUsesPowerLevelsV12` |
| ✅ | – | ✅ | – | `KnockRoomsInPublicRoomsDirectory` |
| ✅ | – | ✅ | – | `KnockRoomsInPublicRoomsDirectoryInMSC3787Room` |
| ✅ | 25/0/0 | ✅ | 25/0/0 | `Knocking` |
| ✅ | 25/0/0 | ✅ | 25/0/0 | `KnockingInMSC3787Room` |
| ✅ | – | ✅ | – | `LeakyTyping` |
| ✅ | – | ✅ | – | `LeaveEventInviteRejection` |
| ❌ | – | ✅ | – | `LeaveEventVisibility` |
| 🟨 | 2/3/0 | ✅ | 5/0/0 | `LeftRoomFixture` |
| ✅ | 2/0/0 | 🟨 | 1/1/0 | `LocalPngThumbnail` |
| ✅ | 8/0/0 | ✅ | 8/0/0 | `Login` |
| ✅ | 4/0/0 | ✅ | 4/0/0 | `Logout` |
| ❌ | – | ✅ | 11/0/0 | `MSC3757OwnedState` |
| ✅ | – | ✅ | – | `MSC3967` |
| ✅ | 11/0/0 | ✅ | 11/0/0 | `MSC4289PrivilegedRoomCreators` |
| ✅ | – | ✅ | – | `MSC4289PrivilegedRoomCreators_Additional` |
| ✅ | – | ✅ | – | `MSC4289PrivilegedRoomCreators_AdditionalCreatorsAndInvited` |
| ✅ | 5/0/0 | ✅ | 5/0/0 | `MSC4289PrivilegedRoomCreators_AdditionalValidation` |
| ✅ | – | ✅ | – | `MSC4289PrivilegedRoomCreators_InvitedAreCreators` |
| ✅ | – | ✅ | – | `MSC4289PrivilegedRoomCreators_Upgrades` |
| ✅ | – | ✅ | – | `MSC4291RoomIDAsHashOfCreateEvent` |
| ✅ | – | ✅ | – | `MSC4291RoomIDAsHashOfCreateEvent_AuthEventsOmitsCreateEvent` |
| ✅ | – | ✅ | – | `MSC4291RoomIDAsHashOfCreateEvent_CannotSendCreateEvent` |
| ✅ | – | ✅ | – | `MSC4291RoomIDAsHashOfCreateEvent_RoomIDIsOnCreateEvent` |
| ✅ | – | ✅ | – | `MSC4291RoomIDAsHashOfCreateEvent_UpgradedRooms` |
| ✅ | – | ✅ | – | `MSC4297StateResolutionV2_1_includes_conflicted_subgraph` |
| ✅ | – | ✅ | – | `MSC4297StateResolutionV2_1_starts_from_empty_set` |
| ❌ | 0/2/0 | ✅ | 2/0/0 | `MSC4308ThreadSubscriptionsSlidingSync` |
| ✅ | – | ✅ | – | `MSC4311FullCreateEventOnStrippedState` |
| ✅ | – | ✅ | – | `MediaConfig` |
| 🟨 | 19/6/0 | 🟨 | 12/13/0 | `MediaFilenames` |
| ✅ | 4/0/0 | 🟨 | 1/3/0 | `MediaWithoutFileName` |
| ✅ | 4/0/0 | ✅ | 4/0/0 | `MediaWithoutFileNameCSMediaV1` |
| 🟨 | 3/2/0 | ✅ | 5/0/0 | `MembersLocal` |
| ❌ | – | ✅ | – | `MembershipOnEvents` |
| ✅ | 5/0/0 | 🟨 | 3/2/0 | `MessagesOverFederation` |
| ✅ | – | ✅ | – | `NetworkPartitionOrdering` |
| ✅ | – | ✅ | – | `NotPresentUserCannotBanOthers` |
| ✅ | – | ✅ | – | `OlderLeftRoomsNotInLeaveSection` |
| ❌ | – | ✅ | – | `OutboundFederationEventSizeGetMissingEvents` |
| ❌ | – | ✅ | – | `OutboundFederationIgnoresMissingEventWithBadJSONForRoomVersion6` |
| ✅ | 1/0/0 | ✅ | 1/0/0 | `OutboundFederationProfile` |
| ✅ | – | ✅ | – | `OutboundFederationSend` |
| ❌ | 0/58/7 | 🟨 | 58/1/6 | `PartialStateJoin` |
| ✅ | 1/0/0 | ✅ | 1/0/0 | `PollsLocalPushRules` |
| ✅ | 3/0/0 | ✅ | 3/0/0 | `PowerLevels` |
| 🟨 | 4/1/0 | ✅ | 5/0/0 | `Presence` |
| ✅ | – | ✅ | – | `PresenceSyncDifferentRooms` |
| ✅ | 2/0/0 | ✅ | 2/0/0 | `ProfileAvatarURL` |
| ✅ | 2/0/0 | ✅ | 2/0/0 | `ProfileDisplayName` |
| ✅ | 9/0/0 | ✅ | 9/0/0 | `PublicRooms` |
| ✅ | – | ✅ | – | `PushRuleCacheHealth` |
| 🟨 | 3/2/0 | 🟨 | 3/2/0 | `PushRuleRoomUpgrade` |
| ✅ | 5/0/0 | ✅ | 5/0/0 | `PushSync` |
| ✅ | 1/0/0 | ✅ | 1/0/0 | `Redact` |
| ✅ | 19/0/4 | ✅ | 23/0/0 | `Registration` |
| ✅ | – | ✅ | – | `Relations` |
| ✅ | – | ✅ | – | `RelationsPagination` |
| ✅ | – | ✅ | – | `RelationsPaginationSync` |
| ✅ | – | ✅ | – | `RemoteAliasRequestsUnderstandUnicode` |
| 🟨 | 1/1/0 | 🟨 | 1/1/0 | `RemotePngThumbnail` |
| ❌ | 0/2/0 | 🟨 | 1/1/0 | `RemotePresence` |
| ✅ | – | ✅ | – | `RemoteTyping` |
| ✅ | 4/0/0 | ✅ | 4/0/0 | `RemovingAccountData` |
| ✅ | 1/0/0 | ✅ | 1/0/0 | `RequestEncodingFails` |
| ✅ | 5/0/0 | ✅ | 5/0/0 | `RestrictedRoomsLocalJoin` |
| ✅ | 5/0/0 | ✅ | 5/0/0 | `RestrictedRoomsLocalJoinInMSC3787Room` |
| ✅ | – | ✅ | – | `RestrictedRoomsLocalJoinNoCreatorsUsesPowerLevelsV11` |
| ✅ | – | ✅ | – | `RestrictedRoomsLocalJoinNoCreatorsUsesPowerLevelsV12` |
| ✅ | 5/0/0 | ✅ | 5/0/0 | `RestrictedRoomsRemoteJoin` |
| ✅ | 5/0/0 | ✅ | 5/0/0 | `RestrictedRoomsRemoteJoinInMSC3787Room` |
| ✅ | – | ✅ | – | `RestrictedRoomsRemoteJoinLocalUser` |
| ✅ | – | ✅ | – | `RestrictedRoomsRemoteJoinLocalUserInMSC3787Room` |
| ✅ | – | ✅ | – | `RestrictedRoomsSpacesSummaryFederation` |
| ✅ | – | ✅ | – | `RestrictedRoomsSpacesSummaryLocal` |
| ✅ | 5/0/0 | ✅ | 5/0/0 | `RoomAlias` |
| ✅ | 10/0/0 | ✅ | 10/0/0 | `RoomCanonicalAlias` |
| ✅ | 15/0/0 | ✅ | 15/0/0 | `RoomCreate` |
| 🟨 | 4/2/0 | ✅ | 6/0/0 | `RoomCreationReportsEventsToMyself` |
| 🟨 | 7/2/0 | ✅ | 9/0/0 | `RoomDeleteAlias` |
| 🟨 | 6/2/0 | ✅ | 8/0/0 | `RoomForget` |
| ✅ | – | ✅ | – | `RoomImageRoundtrip` |
| ✅ | 10/0/0 | ✅ | 10/0/0 | `RoomMembers` |
| ✅ | – | ✅ | – | `RoomMessagesLazyLoading` |
| ✅ | – | ✅ | – | `RoomMessagesLazyLoadingLocalUser` |
| ✅ | – | ✅ | – | `RoomReadMarkers` |
| ✅ | 1/0/0 | ✅ | 1/0/0 | `RoomReceipts` |
| ✅ | 5/0/0 | ✅ | 5/0/0 | `RoomSpecificUsernameAtJoin` |
| ✅ | 5/0/0 | ✅ | 5/0/0 | `RoomSpecificUsernameChange` |
| ✅ | 15/0/0 | ✅ | 15/0/0 | `RoomState` |
| ✅ | – | ❌ | – | `RoomSummary` |
| ✅ | 2/0/0 | ✅ | 2/0/0 | `RoomSummaryAllowedRoomIDs` |
| ✅ | 9/0/0 | ✅ | 9/0/0 | `RoomsInvite` |
| ✅ | 7/0/0 | ✅ | 7/0/0 | `Search` |
| ✅ | – | ✅ | – | `SendAndFetchMessage` |
| ✅ | – | ❌ | – | `SendJoinPartialStateResponse` |
| ✅ | – | ✅ | – | `SendMessageWithTxn` |
| ✅ | – | ✅ | – | `ServerCapabilities` |
| ⬛ | – | ✅ | 8/0/0 | `ServerNotices` |
| 🟨 | 10/3/0 | ✅ | 12/0/0 | `Sync` |
| ✅ | 2/0/0 | ✅ | 2/0/0 | `SyncFilter` |
| ✅ | 3/0/0 | ✅ | 3/0/0 | `SyncLeaveSection` |
| ✅ | – | ✅ | – | `SyncOmitsStateChangeOnFilteredEvents` |
| ✅ | 2/0/0 | ✅ | 2/0/0 | `SyncTimelineGap` |
| ✅ | – | ✅ | – | `TentativeEventualJoiningAfterRejecting` |
| 🟨 | 1/7/0 | ✅ | 8/0/0 | `ThreadSubscriptions` |
| ❌ | – | ✅ | – | `ThreadedReceipts` |
| ✅ | – | ✅ | – | `ThreadsEndpoint` |
| ✅ | – | ✅ | – | `ToDeviceMessages` |
| ✅ | 1/0/0 | ✅ | 1/0/0 | `ToDeviceMessagesOverFederation` |
| ✅ | – | ✅ | – | `TxnIdWithRefreshToken` |
| ❌ | – | ✅ | – | `TxnIdempotency` |
| ✅ | – | ✅ | – | `TxnIdempotencyScopedToDevice` |
| ✅ | – | ✅ | – | `TxnInEvent` |
| ✅ | – | ✅ | – | `TxnScopeOnLocalEcho` |
| ✅ | 3/0/0 | ✅ | 3/0/0 | `Typing` |
| ✅ | – | ✅ | – | `UnbanViaInvite` |
| 🟨 | 4/1/0 | ✅ | 5/0/0 | `UnknownEndpoints` |
| ✅ | – | ✅ | – | `UnrejectRejectedEvents` |
| ✅ | 8/0/0 | ✅ | 8/0/0 | `UploadKey` |
| ✅ | – | ✅ | – | `UploadKeyIdempotency` |
| ✅ | – | ✅ | – | `UploadKeyIdempotencyOverlap` |
| ✅ | – | ✅ | – | `UrlPreview` |
| ✅ | – | ✅ | – | `UserAppearsInChangedDeviceListOnJoinOverFederation` |
| ✅ | 1/0/0 | ✅ | 1/0/0 | `VersionStructure` |
| ✅ | 7/0/0 | ✅ | 7/0/0 | `WithoutOwnedState` |
| ✅ | – | ✅ | – | `WriteMDirectAccountData` |
