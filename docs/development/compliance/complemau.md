# Tuwunel Complemau Test Results

Tuwunel runs [Complemau](https://github.com/matrix-construct/complement/tree/tuwunel-changes/tests/complemau), its Complement-based application service acceptance suite. Raw results are committed to `tests/complement/complemau/results.jsonl`.

## Counts

- Test groups: 35. Passing: **88.6%**
  - ✅ `pass`: 31
  - 🟨 `some`: 0
  - ❌ `fail`: 4
  - ⬛ `skip`: 0

- Subtests: 63. Passing: **96.8%**
  - ✅ `pass`: 61
  - ❌ `fail`: 2
  - ⬛ `skip`: 0

## All Top-Level Tests

| Status | Subtests | Test |
|---|---|---|
| ✅ | 6/0/0 | `ComplemauAppservicePDUInterest` |
| ✅ | – | `ComplemauAppservicePDUInterestRemoteSender` |
| ✅ | – | `ComplemauAppserviceReceivesOTKCountsAndFallbackKeys` |
| ❌ | – | `ComplemauAppserviceReceivesPresence` |
| ❌ | – | `ComplemauAppserviceReceivesReceiptOnce` |
| ✅ | – | `ComplemauAppserviceReceivesToDevice` |
| ✅ | – | `ComplemauAppserviceReceivesTyping` |
| ✅ | 15/0/0 | `ComplemauAppserviceRegistrationLoginAndMSC4190Devices` |
| ✅ | – | `ComplemauAppserviceRoomAliasQuery` |
| ✅ | – | `ComplemauAppserviceScopesTypingByInterest` |
| ✅ | – | `ComplemauAppserviceServesDeviceKeys` |
| ✅ | – | `ComplemauAppserviceServesDeviceKeysOverFederation` |
| ✅ | – | `ComplemauAppserviceServesOneTimeKeys` |
| ✅ | 3/0/0 | `ComplemauAppserviceThirdPartyEdges` |
| ✅ | 2/0/0 | `ComplemauAppserviceThirdPartyLookups` |
| ✅ | 2/0/0 | `ComplemauAppserviceThirdPartyProtocols` |
| ❌ | 0/2/0 | `ComplemauAppserviceUserQueries` |
| ✅ | – | `ComplemauDeviceListsRegistrationGateAndDeviceChanges` |
| ✅ | – | `ComplemauDeviceListsRemoteRoomSharer` |
| ✅ | 4/0/0 | `ComplemauDirectoryExcludesAppserviceUsers` |
| ✅ | 5/0/0 | `ComplemauExclusiveNamespaces` |
| ✅ | 13/0/0 | `ComplemauMasqueradeAuthentication` |
| ✅ | 5/0/0 | `ComplemauMasqueradeDoesNotInferPresence` |
| ✅ | 4/0/0 | `ComplemauPingErrorSurfacing` |
| ✅ | – | `ComplemauPingForcesTransactionRetry` |
| ✅ | 2/0/0 | `ComplemauPingRoundTripAndAuthorization` |
| ✅ | – | `ComplemauReceiptBatchingHasNoLoss` |
| ✅ | – | `ComplemauToDeviceBurstExcludesUninterestedAppservice` |
| ❌ | – | `ComplemauToDeviceBurstFansOut` |
| ✅ | – | `ComplemauToDeviceManyRecipients` |
| ✅ | – | `ComplemauTransactionBatchCapAndOrdering` |
| ✅ | – | `ComplemauTransactionDedupAndUniquifier` |
| ✅ | – | `ComplemauTransactionQueuesAreIndependent` |
| ✅ | – | `ComplemauTransactionRetryPreservesIDAndContent` |
| ✅ | – | `ComplemauTransactionSingleInflightAndBatching` |
