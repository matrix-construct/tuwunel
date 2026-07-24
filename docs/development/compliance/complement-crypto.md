# Tuwunel Complement Crypto Test Results

Tuwunel runs the [Complement Crypto](https://github.com/matrix-org/complement-crypto) end-to-end encryption acceptance suite against Matrix Rust SDK clients. Raw results are committed to `tests/complement-crypto/results.jsonl`.

## Counts

- Test groups: 26. Passing: **80.8%**
  - ✅ `pass`: 21
  - 🟨 `some`: 0
  - ❌ `fail`: 0
  - ⬛ `skip`: 5

- Subtests: 29. Passing: **82.8%**
  - ✅ `pass`: 24
  - ❌ `fail`: 0
  - ⬛ `skip`: 5

## All Top-Level Tests

| Status | Subtests | Test |
|---|---|---|
| ✅ | 1/0/0 | `AliceBobEncryptionWorks` |
| ✅ | 1/0/0 | `BackupWrongRecoveryKeyFails` |
| ✅ | 1/0/0 | `BobCanSeeButNotDecryptHistoryInPublicRoom` |
| ✅ | 1/0/0 | `CanBackupKeys` |
| ✅ | 1/0/0 | `CanDecryptMessagesAfterInviteButBeforeJoin` |
| ✅ | 1/0/0 | `ClientRetriesSendToDevice` |
| ✅ | 1/0/0 | `ExistingSessionCannotGetKeysForOfflineServer` |
| ✅ | 1/0/0 | `FailedDeviceKeyDownloadRetries` |
| ✅ | 1/0/0 | `FailedKeysClaimRetries` |
| ✅ | 1/0/0 | `FailedOneTimeKeyUploadRetries` |
| ✅ | 1/0/0 | `FallbackKeyIsUsedIfOneTimeKeysRunOut` |
| ✅ | 1/0/0 | `NewUserCannotGetKeysForOfflineServer` |
| ✅ | 1/0/0 | `OnNewDeviceBobCanSeeButNotDecryptHistoryInPublicRoom` |
| ✅ | 1/0/0 | `RoomKeyIsCycledAfterEnoughMessages` |
| ✅ | 1/0/0 | `RoomKeyIsCycledAfterEnoughTime` |
| ✅ | 1/0/0 | `RoomKeyIsCycledOnDeviceLogout` |
| ✅ | 1/0/0 | `RoomKeyIsCycledOnMemberLeaving` |
| ✅ | 3/0/0 | `RoomKeyIsNotCycled` |
| ⬛ | 0/0/1 | `RoomKeyIsNotCycledOnClientRestart` |
| ⬛ | 0/0/1 | `SigkillBeforeKeysUploadResponse` |
| ✅ | 2/0/0 | `SpoofedEventSenderHandling` |
| ✅ | 1/0/0 | `ToDeviceMessagesAreBatched` |
| ⬛ | 0/0/1 | `ToDeviceMessagesAreProcessedInOrder` |
| ✅ | 1/0/0 | `ToDeviceMessagesArentLostWhenKeysQueryFails` |
| ⬛ | 0/0/1 | `UnprocessedToDeviceMessagesArentLostOnRestart` |
| ⬛ | 0/0/1 | `VerificationSAS` |
