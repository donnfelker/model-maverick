# API black-box timeline (generated)

Per test, in execution order: `sql` lines are DB statements the test itself ran (seeds and assertion reads, sqlc query name; full SQL and args in api-seed-sql.jsonl), `http` lines are recorded requests (full detail in api-recorded.jsonl, matched by test + seq). Consecutive identical sql lines are collapsed with xN. Ids masked as <col>, <ten>, etc. Tests with no DB seed and no HTTP traffic (pure unit tests) are absent here; see fixtures/api/*.json.

## TestTenantCaptureReadEndpointsRequireAuth
```
http public   GET    /v1/debug/captures -> 401 unauthorized   [seq 1]
http public   GET    /v1/debug/captures/cap_x/prompt -> 401 unauthorized   [seq 2]
```

## TestTenantListCapturesShape
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 1]
http public   POST   /v1/recommend -> 200   [seq 2]
sql  ListDebugCapturesByTenant :many
http public   GET    /v1/debug/captures -> 200   [seq 3]
```

## TestTenantListCapturesStrictlyScoped
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 1]
http public   POST   /v1/recommend -> 200   [seq 2]
sql  ListDebugCapturesByTenant :many
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 3]
http public   POST   /v1/recommend -> 200   [seq 4]
sql  ListDebugCapturesByTenant :many
http public   GET    /v1/debug/captures -> 200   [seq 5]
http public   GET    /v1/debug/captures -> 200   [seq 6]
sql  ListDebugCapturesByTenant :many
http public   GET    /v1/debug/captures?window_id=<cw> -> 200   [seq 7]
http public   GET    /v1/debug/captures?tenant_id=<ten> -> 422 invalid_request   [seq 8]
```

## TestTenantListCapturesPagination
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 1]
http public   POST   /v1/recommend -> 200   [seq 2]
http public   POST   /v1/recommend -> 200   [seq 3]
http public   POST   /v1/recommend -> 200   [seq 4]
sql  ListDebugCapturesByTenant :many
http public   GET    /v1/debug/captures?limit=1 -> 200   [seq 5]
http public   GET    /v1/debug/captures?limit=1&cursor=Y2FwXzAxTTNLUVJBS1E2RllOWVg3MzNEMEpCUDNR -> 200   [seq 6]
http public   GET    /v1/debug/captures?limit=1&cursor=Y2FwXzAxTTNLUVJBS1BON01QVzMxRTgwQUdEN0RY -> 200   [seq 7]
http public   GET    /v1/debug/captures?cursor=not%20a%20token -> 422 invalid_request   [seq 8]
```

## TestTenantReadOwnPromptRoundTrip
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 1]
http public   POST   /v1/recommend -> 200   [seq 2]
sql  ListDebugCapturesByTenant :many
http public   GET    /v1/debug/captures/<cap>/prompt -> 200   [seq 3]
sql  ListCaptureAccessLogByCapture :many
```

## TestTenantReadCrossTenant404IndistinguishableAndLogged
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 1]
http public   POST   /v1/recommend -> 200   [seq 2]
sql  ListDebugCapturesByTenant :many
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   GET    /v1/debug/captures/<cap>/prompt -> 404 not_found   [seq 3]
http public   GET    /v1/debug/captures/cap_01UNKNOWNTENANTREAD000000/prompt -> 404 not_found   [seq 4]
sql  ListCaptureAccessLogByCapture :many x3
```

## TestTenantReadExpiredButPresent410
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
sql  CreateRecommendation :one
sql  CreateDebugCapture :one
http public   GET    /v1/debug/captures/<cap>/prompt -> 410 capture_expired   [seq 1]
sql  ListCaptureAccessLogByCapture :many
```

## TestTenantReadHardDeleted410
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
sql  CreateRecommendation :one
sql  CreateDebugCapture :one
sql  DeleteExpiredDebugCaptures :many
sql  InsertCaptureAccessLog :one
http public   GET    /v1/debug/captures/<cap>/prompt -> 410 capture_expired   [seq 1]
sql  ListCaptureAccessLogByCapture :many
```

## TestTenantCaptureReadZeroRetention
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
sql  UpsertTenantConstraints :one
http public   POST   /v1/recommend -> 200   [seq 1]
http public   GET    /v1/debug/captures -> 200   [seq 2]
http public   GET    /v1/debug/captures/cap_01NEVEREXISTED0000000000/prompt -> 404 not_found   [seq 3]
```

## TestCaptureEndpointsRequireAuth
```
http public   POST   /v1/debug/capture-windows -> 401 unauthorized   [seq 1]
http public   DELETE /v1/debug/capture-windows/cw_x -> 401 unauthorized   [seq 2]
http public   POST   /v1/debug/replay/cap_x -> 401 unauthorized   [seq 3]
```

## TestCaptureWindowCreate201
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 1]
```

## TestCaptureWindowCreateScopedFields
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 1]
```

## TestCaptureWindowCreateValidation422
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 422 invalid_request   [seq 1]
http public   POST   /v1/debug/capture-windows -> 422 invalid_request   [seq 2]
http public   POST   /v1/debug/capture-windows -> 422 invalid_request   [seq 3]
http public   POST   /v1/debug/capture-windows -> 422 invalid_request   [seq 4]
http public   POST   /v1/debug/capture-windows -> 422 invalid_request   [seq 5]
http public   POST   /v1/debug/capture-windows -> 422 invalid_request   [seq 6]
http public   POST   /v1/debug/capture-windows -> 422 invalid_request   [seq 7]
http public   POST   /v1/debug/capture-windows -> 422 invalid_request   [seq 8]
http public   POST   /v1/debug/capture-windows -> 422 invalid_request   [seq 9]
sql  ListDebugCapturesByTenant :many
```

## TestCaptureWindowCreateZeroRetention409
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
sql  UpsertTenantConstraints :one
http public   POST   /v1/debug/capture-windows -> 409 zero_retention_conflict   [seq 1]
sql  ListActiveCaptureWindows :many
```

## TestCaptureWindowDeleteEndsEarly
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 1]
http public   DELETE /v1/debug/capture-windows/<cw> -> 204   [seq 2]
http public   POST   /v1/recommend -> 200   [seq 3]
sql  ListDebugCapturesByTenant :many
http public   DELETE /v1/debug/capture-windows/<cw> -> 404 not_found   [seq 4]
```

## TestCaptureWindowDeleteCrossTenant404
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 1]
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   DELETE /v1/debug/capture-windows/<cw> -> 404 not_found   [seq 2]
```

## TestRecommendPromptCapturedUnderActiveWindow
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 1]
http public   POST   /v1/recommend -> 200   [seq 2]
sql  ListDebugCapturesByTenant :many
sql  ListCaptureAccessLogByCapture :many
```

## TestRecommendPromptNotCapturedWithoutWindow
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/recommend -> 200   [seq 1]
sql  ListDebugCapturesByTenant :many
```

## TestRecommendDeclaredPathNeverCaptured
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 1]
http public   POST   /v1/recommend -> 200   [seq 2]
sql  ListDebugCapturesByTenant :many
```

## TestCaptureWindowKeyScope
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
sql  CreateAPIKey :one
http public   POST   /v1/debug/capture-windows -> 201   [seq 1]
http public   POST   /v1/recommend -> 200   [seq 2]
sql  ListDebugCapturesByTenant :many
http public   POST   /v1/recommend -> 200   [seq 3]
sql  ListDebugCapturesByTenant :many
```

## TestCaptureConfidenceBelowSampling
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 1]
http public   POST   /v1/recommend -> 200   [seq 2]
sql  ListDebugCapturesByTenant :many
http public   DELETE /v1/debug/capture-windows/<cw> -> 204   [seq 3]
http public   POST   /v1/debug/capture-windows -> 201   [seq 4]
http public   POST   /v1/recommend -> 200   [seq 5]
sql  ListDebugCapturesByTenant :many
```

## TestCaptureZeroRetentionBeltAndSuspenders
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
sql  UpsertTenantConstraints :one
sql  CreateCaptureWindow :one
http public   POST   /v1/recommend -> 200   [seq 1]
sql  ListDebugCapturesByTenant :many
```

## TestReplayMatch
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 1]
http public   POST   /v1/recommend -> 200   [seq 2]
sql  ListDebugCapturesByTenant :many
http public   POST   /v1/debug/replay/<cap> -> 200   [seq 3]
sql  ListCaptureAccessLogByCapture :many
```

## TestReplayMismatch
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 1]
http public   POST   /v1/recommend -> 200   [seq 2]
sql  ListDebugCapturesByTenant :many
http public   POST   /v1/debug/replay/<cap> -> 200   [seq 3]
```

## TestReplayClassifierFailureFallsBack
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 1]
http public   POST   /v1/recommend -> 200   [seq 2]
sql  ListDebugCapturesByTenant :many
http public   POST   /v1/debug/replay/<cap> -> 200   [seq 3]
```

## TestReplayClassifierVersion
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 1]
http public   POST   /v1/recommend -> 200   [seq 2]
sql  ListDebugCapturesByTenant :many
http public   POST   /v1/debug/replay/<cap> -> 200   [seq 3]
http public   POST   /v1/debug/replay/<cap> -> 422 invalid_request   [seq 4]
```

## TestReplayNotFound404
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/replay/cap_01UNKNOWN -> 404 not_found   [seq 1]
```

## TestReplayCrossTenant404
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 1]
http public   POST   /v1/recommend -> 200   [seq 2]
sql  ListDebugCapturesByTenant :many
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/replay/<cap> -> 404 not_found   [seq 3]
sql  ListCaptureAccessLogByCapture :many
```

## TestReplayExpiredButPresent410
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
sql  CreateRecommendation :one
sql  CreateDebugCapture :one
http public   POST   /v1/debug/replay/<cap> -> 410 capture_expired   [seq 1]
sql  ListCaptureAccessLogByCapture :many
```

## TestReplayHardDeleted410
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
sql  CreateRecommendation :one
sql  CreateDebugCapture :one
sql  DeleteExpiredDebugCaptures :many
sql  InsertCaptureAccessLog :one
http public   POST   /v1/debug/replay/<cap> -> 410 capture_expired   [seq 1]
```

## TestConfigPoolsRoundTrip
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one x2
http public   GET    /v1/config/pools -> 200   [seq 1]
http public   GET    /v1/config/pools -> 200   [seq 2]
http public   PUT    /v1/config/pools -> 200   [seq 3]
http public   GET    /v1/config/pools -> 200   [seq 4]
```

## TestConfigPoolsFullReplace
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
http public   PUT    /v1/config/pools -> 200   [seq 1]
http public   PUT    /v1/config/pools -> 200   [seq 2]
http public   GET    /v1/config/pools -> 200   [seq 3]
```

## TestConfigPoolsUnknownModel
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   PUT    /v1/config/pools -> 422 unknown_model   [seq 1]
http public   GET    /v1/config/pools -> 200   [seq 2]
```

## TestConfigPoolsDefaultRule
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
http public   PUT    /v1/config/pools -> 422 invalid_request   [seq 1]
http public   PUT    /v1/config/pools -> 422 invalid_request   [seq 2]
http public   PUT    /v1/config/pools -> 422 invalid_request   [seq 3]
```

## TestConfigPoolsDuplicateName
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
http public   PUT    /v1/config/pools -> 422 invalid_request   [seq 1]
```

## TestConfigPoolsAllRetired
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
http public   PUT    /v1/config/pools -> 422 pool_would_be_empty   [seq 1]
```

## TestConfigPoolsRejectedByStoredConstraint
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one x2
sql  UpsertAggregate :one x2
http public   PUT    /v1/config/pools -> 200   [seq 1]
http public   PUT    /v1/config/constraints -> 200   [seq 2]
http public   PUT    /v1/config/pools -> 422 pool_would_be_empty   [seq 3]
http public   GET    /v1/config/pools -> 200   [seq 4]
```

## TestConfigConstraintsRoundTrip
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   GET    /v1/config/constraints -> 200   [seq 1]
http public   PUT    /v1/config/constraints -> 200   [seq 2]
http public   GET    /v1/config/constraints -> 200   [seq 3]
http public   PUT    /v1/config/constraints -> 200   [seq 4]
```

## TestConfigConstraintsWouldEmptyPool
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
sql  UpsertAggregate :one
http public   PUT    /v1/config/pools -> 200   [seq 1]
http public   PUT    /v1/config/constraints -> 422 pool_would_be_empty   [seq 2]
http public   GET    /v1/config/constraints -> 200   [seq 3]
```

## TestConfigConstraintsLatencyWouldEmptyPool
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
sql  UpsertAggregate :one
http public   PUT    /v1/config/pools -> 200   [seq 1]
http public   PUT    /v1/config/constraints -> 422 pool_would_be_empty   [seq 2]
```

## TestConfigConstraintsNameTheEmptiedPool
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one x2
sql  UpsertAggregate :one x2
http public   PUT    /v1/config/pools -> 200   [seq 1]
http public   PUT    /v1/config/constraints -> 422 pool_would_be_empty   [seq 2]
```

## TestConfigConstraintsUnmeasuredColumnsSurvive
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
http public   PUT    /v1/config/pools -> 200   [seq 1]
http public   PUT    /v1/config/constraints -> 200   [seq 2]
```

## TestConfigConstraintsUseLatestAggregate
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
sql  UpsertAggregate :one x2
http public   PUT    /v1/config/pools -> 200   [seq 1]
http public   PUT    /v1/config/constraints -> 200   [seq 2]
```

## TestConfigWeightsRoundTrip
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   GET    /v1/config/weights -> 200   [seq 1]
http public   PUT    /v1/config/weights -> 200   [seq 2]
http public   GET    /v1/config/weights -> 200   [seq 3]
```

## TestConfigWeightsFullReplace
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   PUT    /v1/config/weights -> 200   [seq 1]
http public   PUT    /v1/config/weights -> 200   [seq 2]
http public   GET    /v1/config/weights -> 200   [seq 3]
```

## TestConfigWeightsValidation
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   PUT    /v1/config/weights -> 422 invalid_request   [seq 1]
http public   PUT    /v1/config/weights -> 422 invalid_request   [seq 2]
http public   PUT    /v1/config/weights -> 422 invalid_request   [seq 3]
http public   PUT    /v1/config/weights -> 422 invalid_request   [seq 4]
http public   PUT    /v1/config/weights -> 422 invalid_request   [seq 5]
http public   PUT    /v1/config/weights -> 422 invalid_request   [seq 6]
http public   PUT    /v1/config/weights -> 422 invalid_request   [seq 7]
http public   PUT    /v1/config/weights -> 422 invalid_request   [seq 8]
http public   PUT    /v1/config/weights -> 422 invalid_request   [seq 9]
http public   GET    /v1/config/weights -> 200   [seq 10]
```

## TestConfigWeightsSumTolerance
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   PUT    /v1/config/weights -> 422 invalid_request   [seq 1]
http public   PUT    /v1/config/weights -> 200   [seq 2]
```

## TestConfigStrictDecoding
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
http public   PUT    /v1/config/pools -> 422 invalid_request   [seq 1]
http public   PUT    /v1/config/pools -> 422 invalid_request   [seq 2]
http public   PUT    /v1/config/pools -> 422 invalid_request   [seq 3]
http public   PUT    /v1/config/constraints -> 422 invalid_request   [seq 4]
http public   PUT    /v1/config/constraints -> 422 invalid_request   [seq 5]
http public   PUT    /v1/config/pools -> 422 invalid_request   [seq 6]
http public   PUT    /v1/config/pools -> 422 invalid_request   [seq 7]
http public   PUT    /v1/config/pools -> 422 invalid_request   [seq 8]
http public   PUT    /v1/config/weights -> 422 invalid_request   [seq 9]
http public   PUT    /v1/config/constraints -> 422 invalid_request   [seq 10]
http public   PUT    /v1/config/pools -> 422 invalid_request   [seq 11]
http public   PUT    /v1/config/pools -> 422 invalid_request   [seq 12]
```

## TestConfigCrossTenantIsolation
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
http public   PUT    /v1/config/pools -> 200   [seq 1]
http public   GET    /v1/config/pools -> 200   [seq 2]
http public   PUT    /v1/config/pools -> 200   [seq 3]
http public   GET    /v1/config/pools -> 200   [seq 4]
```

## TestConfigOversizeBodyDeclaredLength413
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   PUT    /v1/config/pools -> 413 payload_too_large   [seq 1]
```

## TestConfigOversizeBodyChunked413
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   PUT    /v1/config/pools -> 413 payload_too_large   [seq 1]
```

## TestConfigWrongMethod
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   POST   /v1/config/pools -> 404 not_found   [seq 1]
```

## TestConfigRequiresAuth
```
http public   GET    /v1/config/pools -> 401 unauthorized   [seq 1]
http public   GET    /v1/config/weights -> 401 unauthorized   [seq 2]
http public   GET    /v1/config/constraints -> 401 unauthorized   [seq 3]
```

## TestPhase2ExitEndToEndRecommend
```
sql  begin
sql  UpsertCorpusVersion :one
sql  UpsertTask :one x4
sql  DeactivateCorpusTasksNotIn :execrows
sql  commit
sql  begin
sql  AcquireCorpusActivationLock :exec
sql  ActivateCorpusVersion :execrows
sql  commit
sql  ListActiveTasksByCorpusVersion :many
sql  CreateModelColumn :one x2
sql  SELECT 1
sql  SELECT coalesce(current_schema(), '');
sql  LISTEN "public.river_leadership"
sql  QueueCreateOrSetUpdatedAt :one
sql  begin
sql  JobInsertFastMany :many
sql  PGNotifyMany :exec
sql  commit
sql  begin
sql  JobInsertFastMany :many
sql  PGNotifyMany :exec
sql  commit
sql  begin
sql  JobInsertFastMany :many
sql  commit
sql  GetLatestAggregate :one x7
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   PUT    /v1/config/pools -> 200   [seq 1]
http public   POST   /v1/recommend -> 200   [seq 2]
sql  GetRecommendation :one
```

## TestHistoryRequiresAuth
```
http public   GET    /v1/history/recommendations -> 401 unauthorized   [seq 1]
http public   GET    /v1/history/outcomes -> 401 unauthorized   [seq 2]
http public   GET    /v1/history/calibration -> 401 unauthorized   [seq 3]
http public   GET    /v1/history/models/col_01X -> 401 unauthorized   [seq 4]
http public   GET    /v1/history/recommendations/rec_01X -> 401 unauthorized   [seq 5]
```

## TestHistoryRecommendationsSummaries
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateRecommendation :one x2
http public   GET    /v1/history/recommendations -> 200   [seq 1]
```

## TestHistoryRecommendationsFilters
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateRecommendation :one x3
http public   GET    /v1/history/recommendations?prompt_hash=hmac_sha256%3A9f2c -> 200   [seq 1]
http public   GET    /v1/history/recommendations?capability=code_generation -> 200   [seq 2]
http public   GET    /v1/history/recommendations?since=2026-09-28T06%3A07%3A51.128487-04%3A00 -> 200   [seq 3]
http public   GET    /v1/history/recommendations?capability=nope -> 422 invalid_request   [seq 4]
```

## TestHistoryRecommendationsPaginationWalk
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateRecommendation :one x5
http public   GET    /v1/history/recommendations?limit=2 -> 200   [seq 1]
http public   GET    /v1/history/recommendations?limit=2&cursor=djF8cmVjfHJlY18wMU0zS1FSQ0FaUU05TVZZNk42M0dTREtTMA -> 200   [seq 2]
http public   GET    /v1/history/recommendations?limit=2&cursor=djF8cmVjfHJlY18wMU0zS1FSQ0FZSDdOQzdTS041NjhQMjNKVw -> 200   [seq 3]
```

## TestHistoryRecommendationsPaginationExactBoundary
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateRecommendation :one x4
http public   GET    /v1/history/recommendations?limit=2 -> 200   [seq 1]
http public   GET    /v1/history/recommendations?limit=2&cursor=djF8cmVjfHJlY18wMU0zS1FSQ0I1QkQ4TVdHVFFSRjNLODZSRw -> 200   [seq 2]
```

## TestHistoryRecommendationsEmptyPage
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   GET    /v1/history/recommendations -> 200   [seq 1]
```

## TestHistoryRecommendationsCursorStableUnderInserts
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateRecommendation :one x4
http public   GET    /v1/history/recommendations?limit=2 -> 200   [seq 1]
sql  CreateRecommendation :one
http public   GET    /v1/history/recommendations?limit=2&cursor=djF8cmVjfHJlY18wMU0zS1FSQ0JFNEc2NFQwSzhDUFg5QkRDRg -> 200   [seq 2]
```

## TestHistoryRecommendationsCrossTenantIsolation
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateRecommendation :one
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   GET    /v1/history/recommendations -> 200   [seq 1]
```

## TestHistoryInvalidCursor422
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
http public   GET    /v1/history/recommendations?cursor=%21%21garbage -> 422 invalid_request   [seq 1]
http public   GET    /v1/history/outcomes?cursor=%21%21garbage -> 422 invalid_request   [seq 2]
http public   GET    /v1/history/calibration?cursor=%21%21garbage -> 422 invalid_request   [seq 3]
http public   GET    /v1/history/models/<col>?cursor=%21%21garbage -> 422 invalid_request   [seq 4]
```

## TestHistoryForeignCursor422
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateRecommendation :one x2
http public   GET    /v1/history/recommendations?limit=1 -> 200   [seq 1]
http public   GET    /v1/history/outcomes?cursor=djF8cmVjfHJlY18wMU0zS1FSQ0JXTlZRUE5FUkU0OEJYMzJFWQ -> 422 invalid_request   [seq 2]
```

## TestHistoryInvalidLimit422
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   GET    /v1/history/recommendations?limit=0 -> 422 invalid_request   [seq 1]
http public   GET    /v1/history/recommendations?limit=-1 -> 422 invalid_request   [seq 2]
http public   GET    /v1/history/recommendations?limit=201 -> 422 invalid_request   [seq 3]
http public   GET    /v1/history/recommendations?limit=abc -> 422 invalid_request   [seq 4]
http public   GET    /v1/history/recommendations?limit=2.5 -> 422 invalid_request   [seq 5]
```

## TestHistoryLimitBoundsAccepted
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   GET    /v1/history/recommendations?limit=1 -> 200   [seq 1]
http public   GET    /v1/history/recommendations?limit=200 -> 200   [seq 2]
```

## TestHistoryUnknownParam422
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   GET    /v1/history/recommendations?offset=10 -> 422 invalid_request   [seq 1]
```

## TestHistoryInvalidSince422
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   GET    /v1/history/recommendations?since=yesterday -> 422 invalid_request   [seq 1]
```

## TestHistoryTraceFullDecision
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateRecommendation :one
http public   GET    /v1/history/recommendations/<rec> -> 200   [seq 1]
```

## TestHistoryTraceNullPromptHashOnDeclared
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateRecommendation :one
http public   GET    /v1/history/recommendations/<rec> -> 200   [seq 1]
```

## TestHistoryTraceCrossTenant404Oracle
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateRecommendation :one
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   GET    /v1/history/recommendations/<rec> -> 404 not_found   [seq 1]
http public   GET    /v1/history/recommendations/rec_01NEVEREXISTED -> 404 not_found   [seq 2]
```

## TestHistoryOutcomesListAndFilters
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateRecommendation :one x2
http public   POST   /v1/telemetry -> 200   [seq 1]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 2]
http public   POST   /v1/telemetry -> 200   [seq 3]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 4]
http public   GET    /v1/history/outcomes -> 200   [seq 5]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 6]
http public   GET    /v1/history/outcomes?outcome=success -> 200   [seq 7]
http public   GET    /v1/history/outcomes?outcome=shrug -> 422 invalid_request   [seq 8]
```

## TestHistoryOutcomesCrossTenantIsolation
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateRecommendation :one
http public   POST   /v1/telemetry -> 200   [seq 1]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 2]
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   GET    /v1/history/outcomes -> 200   [seq 3]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 4]
```

## TestHistoryOutcomesPaginationWalk
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateRecommendation :one
http public   POST   /v1/telemetry -> 200   [seq 1]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 2]
http public   POST   /v1/telemetry -> 200   [seq 3]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 4]
http public   POST   /v1/telemetry -> 200   [seq 5]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 6]
http public   GET    /v1/history/outcomes?limit=2 -> 200   [seq 7]
http public   GET    /v1/history/outcomes?limit=2&cursor=djF8b3V0fG91dF8wMU0zS1FSQ0QzNlNFS1FSREJOVDJCUTBFOA -> 200   [seq 8]
```

## TestHistoryModelSeriesShapeAndOrder
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  UpsertAggregate :one x4
http public   GET    /v1/history/models/<col> -> 200   [seq 1]
```

## TestHistoryModelSeriesRegressionMarker
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
sql  UpsertAggregate :one x2
sql  InsertAlert :one x3
http public   GET    /v1/history/models/<col> -> 200   [seq 1]
```

## TestHistoryModelSeriesFilters
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
sql  UpsertAggregate :one x4
http public   GET    /v1/history/models/<col>?capability=code_generation -> 200   [seq 1]
http public   GET    /v1/history/models/<col>?difficulty=4 -> 200   [seq 2]
http public   GET    /v1/history/models/<col>?dimension=latency_p95_ms -> 200   [seq 3]
sql  UpsertAggregate :one
http public   GET    /v1/history/models/<col>?since=2026-07-02 -> 200   [seq 4]
http public   GET    /v1/history/models/<col>?capability=nope -> 422 invalid_request   [seq 5]
http public   GET    /v1/history/models/<col>?tier=3 -> 422 invalid_request   [seq 6]
http public   GET    /v1/history/models/<col>?dimension=vibes -> 422 invalid_request   [seq 7]
http public   GET    /v1/history/models/<col>?difficulty=6 -> 422 invalid_request   [seq 8]
http public   GET    /v1/history/models/<col>?difficulty=three -> 422 invalid_request   [seq 9]
```

## TestHistoryModelSeriesPaginationWalk
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
sql  UpsertAggregate :one x6
http public   GET    /v1/history/models/<col>?limit=4 -> 200   [seq 1]
http public   GET    /v1/history/models/<col>?limit=4&cursor=djF8YWdnfDIwMjYtMDctMDJ8c3RydWN0dXJlZF9leHRyYWN0aW9ufDM -> 200   [seq 2]
```

## TestHistoryModelSeriesUnknownColumn404
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   GET    /v1/history/models/col_01NOPE -> 404 not_found   [seq 1]
```

## TestHistoryModelSeriesEmptyForKnownColumn
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
http public   GET    /v1/history/models/<col> -> 200   [seq 1]
```

## TestHistoryCalibrationListShape
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCalibration :one x3
http public   GET    /v1/history/calibration?since=2026-09-28T06%3A07%3A51.237853-04%3A00 -> 200   [seq 1]
```

## TestHistoryCalibrationFiltersAndPagination
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCalibration :one x6
http public   GET    /v1/history/calibration?capability=factual_qa&since=2026-09-28T06%3A07%3A51.242058-04%3A00 -> 200   [seq 1]
http public   GET    /v1/history/calibration?capability=nope -> 422 invalid_request   [seq 2]
http public   GET    /v1/history/calibration?limit=4&since=2026-09-28T06%3A07%3A51.242058-04%3A00 -> 200   [seq 3]
http public   GET    /v1/history/calibration?limit=4&since=2026-09-28T06%3A07%3A51.242058-04%3A00&cursor=djF8Y2FsfGJhdF8wMU0zS1FSQ0U5UThHRFBSSzFIMUNDSEJHQnxzdHJ1Y3R1cmVkX2V4dHJhY3Rpb24 -> 200   [seq 4]
```

## TestOperatorCaptureEndpointsRequireAuth
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http internal GET    /internal/v1/captures -> 401 unauthorized   [seq 1]
http internal GET    /internal/v1/captures -> 401 unauthorized   [seq 2]
http internal GET    /internal/v1/captures/cap_x/prompt -> 401 unauthorized   [seq 3]
http internal GET    /internal/v1/captures/cap_x/prompt -> 401 unauthorized   [seq 4]
```

## TestOperatorListCapturesShape
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 1]
http public   POST   /v1/recommend -> 200   [seq 2]
sql  ListDebugCapturesByTenant :many
http internal GET    /internal/v1/captures?tenant_id=<ten> -> 200   [seq 3]
```

## TestOperatorListCapturesFilters
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 1]
http public   POST   /v1/recommend -> 200   [seq 2]
sql  ListDebugCapturesByTenant :many
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 3]
http public   POST   /v1/recommend -> 200   [seq 4]
sql  ListDebugCapturesByTenant :many
http internal GET    /internal/v1/captures -> 200   [seq 5]
http internal GET    /internal/v1/captures?tenant_id=<ten> -> 200   [seq 6]
http internal GET    /internal/v1/captures?window_id=<cw> -> 200   [seq 7]
http internal GET    /internal/v1/captures?bogus=1 -> 422 invalid_request   [seq 8]
```

## TestOperatorListCapturesPagination
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 1]
http public   POST   /v1/recommend -> 200   [seq 2]
http public   POST   /v1/recommend -> 200   [seq 3]
http public   POST   /v1/recommend -> 200   [seq 4]
sql  ListDebugCapturesByTenant :many
http internal GET    /internal/v1/captures?tenant_id=<ten>&limit=1 -> 200   [seq 5]
http internal GET    /internal/v1/captures?tenant_id=<ten>&limit=1&cursor=Y2FwXzAxTTNLUVJDRlJONUFYVFo2V0hNOENNUkFT -> 200   [seq 6]
http internal GET    /internal/v1/captures?tenant_id=<ten>&limit=1&cursor=Y2FwXzAxTTNLUVJDRlFaSFJQTlhFQUFCRVBKRzlX -> 200   [seq 7]
http internal GET    /internal/v1/captures?cursor=not-valid-base64%20%20 -> 422 invalid_request   [seq 8]
```

## TestOperatorReadCapturePromptRoundTrip
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 1]
http public   POST   /v1/recommend -> 200   [seq 2]
sql  ListDebugCapturesByTenant :many
http internal GET    /internal/v1/captures/<cap>/prompt -> 200   [seq 3]
sql  ListCaptureAccessLogByCapture :many
```

## TestOperatorReadCapturePromptExpiredButPresent410
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
sql  CreateRecommendation :one
sql  CreateDebugCapture :one
http internal GET    /internal/v1/captures/<cap>/prompt -> 410 capture_expired   [seq 1]
sql  ListCaptureAccessLogByCapture :many
```

## TestOperatorReadCapturePromptHardDeleted410
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
sql  CreateRecommendation :one
sql  CreateDebugCapture :one
sql  DeleteExpiredDebugCaptures :many
sql  InsertCaptureAccessLog :one
http internal GET    /internal/v1/captures/<cap>/prompt -> 410 capture_expired   [seq 1]
sql  ListCaptureAccessLogByCapture :many
```

## TestOperatorReadCapturePromptUnknown404
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http internal GET    /internal/v1/captures/cap_01UNKNOWNOPERATORREAD0000/prompt -> 404 not_found   [seq 1]
sql  ListCaptureAccessLogByCapture :many
```

## TestOperatorListZeroRetentionTenantHasNoCaptures
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
sql  UpsertTenantConstraints :one
http public   POST   /v1/recommend -> 200   [seq 1]
http internal GET    /internal/v1/captures?tenant_id=<ten> -> 200   [seq 2]
```

## TestPublicListenerNeverLeaksForeignPromptText
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/debug/capture-windows -> 201   [seq 1]
http public   POST   /v1/recommend -> 200   [seq 2]
sql  ListDebugCapturesByTenant :many
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   GET    /v1/debug/captures/<cap>/prompt -> 404 not_found   [seq 3]
http public   GET    /v1/debug/capture-windows/<cap>/prompt -> 404 not_found   [seq 4]
http public   GET    /v1/debug/captures -> 200   [seq 5]
http public   GET    /v1/debug/captures -> 200   [seq 6]
http public   POST   /v1/debug/replay/<cap> -> 200   [seq 7]
```

## TestInternalV1EveryEndpointRejectsMissingAndBadTokens
```
http internal POST   /internal/v1/tenants -> 401 unauthorized   [seq 1]
http internal POST   /internal/v1/tenants -> 401 unauthorized   [seq 2]
http internal POST   /internal/v1/tenants -> 401 unauthorized   [seq 3]
http internal GET    /internal/v1/tenants/ten_x -> 401 unauthorized   [seq 4]
http internal GET    /internal/v1/tenants/ten_x -> 401 unauthorized   [seq 5]
http internal GET    /internal/v1/tenants/ten_x -> 401 unauthorized   [seq 6]
http internal POST   /internal/v1/tenants/ten_x/suspend -> 401 unauthorized   [seq 7]
http internal POST   /internal/v1/tenants/ten_x/suspend -> 401 unauthorized   [seq 8]
http internal POST   /internal/v1/tenants/ten_x/suspend -> 401 unauthorized   [seq 9]
http internal POST   /internal/v1/tenants/ten_x/activate -> 401 unauthorized   [seq 10]
http internal POST   /internal/v1/tenants/ten_x/activate -> 401 unauthorized   [seq 11]
http internal POST   /internal/v1/tenants/ten_x/activate -> 401 unauthorized   [seq 12]
http internal POST   /internal/v1/tenants/ten_x/api-keys -> 401 unauthorized   [seq 13]
http internal POST   /internal/v1/tenants/ten_x/api-keys -> 401 unauthorized   [seq 14]
http internal POST   /internal/v1/tenants/ten_x/api-keys -> 401 unauthorized   [seq 15]
http internal DELETE /internal/v1/api-keys/key_x -> 401 unauthorized   [seq 16]
http internal DELETE /internal/v1/api-keys/key_x -> 401 unauthorized   [seq 17]
http internal DELETE /internal/v1/api-keys/key_x -> 401 unauthorized   [seq 18]
http internal PUT    /internal/v1/tenants/ten_x/pools -> 401 unauthorized   [seq 19]
http internal PUT    /internal/v1/tenants/ten_x/pools -> 401 unauthorized   [seq 20]
http internal PUT    /internal/v1/tenants/ten_x/pools -> 401 unauthorized   [seq 21]
http internal PUT    /internal/v1/tenants/ten_x/weights -> 401 unauthorized   [seq 22]
http internal PUT    /internal/v1/tenants/ten_x/weights -> 401 unauthorized   [seq 23]
http internal PUT    /internal/v1/tenants/ten_x/weights -> 401 unauthorized   [seq 24]
http internal PUT    /internal/v1/tenants/ten_x/constraints -> 401 unauthorized   [seq 25]
http internal PUT    /internal/v1/tenants/ten_x/constraints -> 401 unauthorized   [seq 26]
http internal PUT    /internal/v1/tenants/ten_x/constraints -> 401 unauthorized   [seq 27]
http internal GET    /internal/v1/alerts -> 401 unauthorized   [seq 28]
http internal GET    /internal/v1/alerts -> 401 unauthorized   [seq 29]
http internal GET    /internal/v1/alerts -> 401 unauthorized   [seq 30]
http internal POST   /internal/v1/columns -> 401 unauthorized   [seq 31]
http internal POST   /internal/v1/columns -> 401 unauthorized   [seq 32]
http internal POST   /internal/v1/columns -> 401 unauthorized   [seq 33]
http internal PUT    /internal/v1/columns/col_x -> 401 unauthorized   [seq 34]
http internal PUT    /internal/v1/columns/col_x -> 401 unauthorized   [seq 35]
http internal PUT    /internal/v1/columns/col_x -> 401 unauthorized   [seq 36]
http internal POST   /internal/v1/corpus/activate -> 401 unauthorized   [seq 37]
http internal POST   /internal/v1/corpus/activate -> 401 unauthorized   [seq 38]
http internal POST   /internal/v1/corpus/activate -> 401 unauthorized   [seq 39]
```

## TestInternalV1EachRotationTokenAcceptedWithAuditActor
```
http internal POST   /internal/v1/tenants -> 201   [seq 1]
http internal POST   /internal/v1/tenants -> 201   [seq 2]
```

## TestInternalTenantCreateDefaults
```
sql  CreateModelColumn :one x4
http internal POST   /internal/v1/tenants -> 201   [seq 1]
sql  ListModelColumnsByStatuses :many
sql  GetTenant :one
```

## TestInternalTenantCreateWithoutActiveColumnsHasNoPool
```
sql  ListModelColumnsByStatuses :many
sql  UpdateModelColumn :one x67
http internal POST   /internal/v1/tenants -> 201   [seq 1]
```

## TestInternalTenantCreateValidation
```
http internal POST   /internal/v1/tenants -> 422 invalid_request   [seq 1]
http internal POST   /internal/v1/tenants -> 422 invalid_request   [seq 2]
http internal POST   /internal/v1/tenants -> 422 invalid_request   [seq 3]
http internal POST   /internal/v1/tenants -> 422 invalid_request   [seq 4]
```

## TestInternalTenantGetSummary
```
sql  CreateModelColumn :one
http internal POST   /internal/v1/tenants -> 201   [seq 1]
http internal GET    /internal/v1/tenants/<ten> -> 200   [seq 2]
http internal GET    /internal/v1/tenants/<ten> -> 404 not_found   [seq 3]
```

## TestInternalTenantSuspendActivateToggleReachesPublicListener
```
sql  CreateModelColumn :one
http internal POST   /internal/v1/tenants -> 201   [seq 1]
http internal POST   /internal/v1/tenants/<ten>/api-keys -> 201   [seq 2]
http public   GET    /v1/models -> 200   [seq 3]
http internal POST   /internal/v1/tenants/<ten>/suspend -> 200   [seq 4]
http public   GET    /v1/models -> 403 tenant_suspended   [seq 5]
http internal POST   /internal/v1/tenants/<ten>/activate -> 200   [seq 6]
http public   GET    /v1/models -> 200   [seq 7]
http internal POST   /internal/v1/tenants/ten_missing/suspend -> 404 not_found   [seq 8]
http internal POST   /internal/v1/tenants/ten_missing/activate -> 404 not_found   [seq 9]
```

## TestInternalAPIKeyRawReturnedExactlyOnce
```
sql  CreateModelColumn :one
http internal POST   /internal/v1/tenants -> 201   [seq 1]
http internal POST   /internal/v1/tenants/<ten>/api-keys -> 201   [seq 2]
http public   GET    /v1/models -> 200   [seq 3]
http internal GET    /internal/v1/tenants/<ten> -> 200   [seq 4]
http internal DELETE /internal/v1/api-keys/<key> -> 200   [seq 5]
http public   GET    /v1/models -> 401 unauthorized   [seq 6]
http internal DELETE /internal/v1/api-keys/<key> -> 200   [seq 7]
http internal DELETE /internal/v1/api-keys/key_missing -> 404 not_found   [seq 8]
http internal POST   /internal/v1/tenants/ten_missing/api-keys -> 404 not_found   [seq 9]
http internal POST   /internal/v1/tenants/<ten>/api-keys -> 422 invalid_request   [seq 10]
```

## TestInternalConfigG4ParityByteExact
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one
http public   PUT    /v1/config/pools -> 422 unknown_model   [seq 1]
http internal PUT    /internal/v1/tenants/<ten>/pools -> 422 unknown_model   [seq 2]
http public   PUT    /v1/config/pools -> 422 invalid_request   [seq 3]
http internal PUT    /internal/v1/tenants/<ten>/pools -> 422 invalid_request   [seq 4]
http public   PUT    /v1/config/weights -> 422 invalid_request   [seq 5]
http internal PUT    /internal/v1/tenants/<ten>/weights -> 422 invalid_request   [seq 6]
http public   PUT    /v1/config/constraints -> 422 pool_would_be_empty   [seq 7]
http internal PUT    /internal/v1/tenants/<ten>/constraints -> 422 pool_would_be_empty   [seq 8]
sql  CreateModelColumn :one
http internal PUT    /internal/v1/tenants/<ten>/pools -> 200   [seq 9]
http public   GET    /v1/config/pools -> 200   [seq 10]
http internal PUT    /internal/v1/tenants/<ten>/weights -> 200   [seq 11]
http internal PUT    /internal/v1/tenants/<ten>/constraints -> 200   [seq 12]
http internal PUT    /internal/v1/tenants/ten_missing/pools -> 404 not_found   [seq 13]
http internal PUT    /internal/v1/tenants/ten_missing/weights -> 404 not_found   [seq 14]
http internal PUT    /internal/v1/tenants/ten_missing/constraints -> 404 not_found   [seq 15]
```

## TestInternalColumnCreateEntersColdStart
```
http internal POST   /internal/v1/columns -> 201   [seq 1]
http internal POST   /internal/v1/columns -> 422 invalid_request   [seq 2]
http internal POST   /internal/v1/columns -> 422 invalid_request   [seq 3]
http internal POST   /internal/v1/columns -> 422 invalid_request   [seq 4]
http internal POST   /internal/v1/columns -> 422 invalid_request   [seq 5]
http internal POST   /internal/v1/columns -> 422 invalid_request   [seq 6]
http internal POST   /internal/v1/columns -> 422 invalid_request   [seq 7]
http internal POST   /internal/v1/columns -> 422 invalid_request   [seq 8]
http internal POST   /internal/v1/columns -> 422 invalid_request   [seq 9]
http internal POST   /internal/v1/columns -> 422 invalid_request   [seq 10]
http internal POST   /internal/v1/columns -> 422 invalid_request   [seq 11]
http internal POST   /internal/v1/columns -> 422 invalid_request   [seq 12]
http internal POST   /internal/v1/columns -> 422 invalid_request   [seq 13]
http internal POST   /internal/v1/columns -> 422 invalid_request   [seq 14]
http internal POST   /internal/v1/columns -> 201   [seq 15]
```

## TestInternalColumnUpdateStatusPricingDeprecation
```
sql  CreateModelColumn :one
http internal PUT    /internal/v1/columns/<col> -> 200   [seq 1]
http internal PUT    /internal/v1/columns/<col> -> 200   [seq 2]
http internal PUT    /internal/v1/columns/col_missing -> 404 not_found   [seq 3]
http internal PUT    /internal/v1/columns/<col> -> 422 invalid_request   [seq 4]
```

## TestInternalColumnExecutionAliasesCreateAndUpdate
```
http internal POST   /internal/v1/columns -> 201   [seq 1]
http internal PUT    /internal/v1/columns/<col> -> 200   [seq 2]
http internal PUT    /internal/v1/columns/<col> -> 200   [seq 3]
```

## TestInternalColumnRetireGuardBlocksThenAllowsAfterAmend
```
sql  CreateTenant :one
sql  CreateModelColumn :one
sql  CreatePool :one
http internal PUT    /internal/v1/columns/<col> -> 422 pool_would_be_empty   [seq 1]
sql  GetModelColumn :one
sql  ListAlertsByKind :many
sql  CreateModelColumn :one
http internal PUT    /internal/v1/tenants/<ten>/pools -> 200   [seq 2]
http internal PUT    /internal/v1/columns/<col> -> 200   [seq 3]
```

## TestInternalColumnRetireBlockDeliversAlert
```
sql  CreateTenant :one
sql  CreateModelColumn :one
sql  CreatePool :one
http internal PUT    /internal/v1/columns/<col> -> 422 pool_would_be_empty   [seq 1]
```

## TestInternalAlertsFeedFilters
```
sql  InsertAlert :one x2
http internal GET    /internal/v1/alerts -> 200   [seq 1]
http internal GET    /internal/v1/alerts?kind=spend_warning -> 200   [seq 2]
http internal GET    /internal/v1/alerts?since=2026-09-28T10:07:51.416239Z -> 200   [seq 3]
http internal GET    /internal/v1/alerts?since=notatime -> 422 invalid_request   [seq 4]
http internal GET    /internal/v1/alerts?bogus=1 -> 422 invalid_request   [seq 5]
```

## TestInternalCorpusActivateFlipsActiveVersion
```
sql  UpsertCorpusVersion :one x2
http internal POST   /internal/v1/corpus/activate -> 200   [seq 1]
sql  GetActiveCorpusVersion :one
sql  GetCorpusVersion :one
http internal POST   /internal/v1/corpus/activate -> 404 not_found   [seq 2]
http internal POST   /internal/v1/corpus/activate -> 422 invalid_request   [seq 3]
sql  UpsertCorpusVersion :one
http internal POST   /internal/v1/corpus/activate -> 200   [seq 4]
```

## TestInternalProvisionThenRecommendEndToEnd
```
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one x2
sql  UpsertAggregate :one x16
http internal POST   /internal/v1/tenants -> 201   [seq 1]
http internal POST   /internal/v1/tenants/<ten>/api-keys -> 201   [seq 2]
http public   POST   /v1/recommend -> 200   [seq 3]
```

## TestInternalV1UnknownPathAndMethodEnvelopes
```
http internal GET    /internal/v1/nope -> 404 not_found   [seq 1]
http internal DELETE /internal/v1/tenants -> 404 not_found   [seq 2]
```

## TestMCPRecommendModelMatchesRESTAndWritesTraceRow
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one x3
sql  CreatePool :one
sql  UpsertAggregate :one x24
http public   POST   /v1/recommend -> 200   [seq 1]
http public   POST   /mcp -> 200   [seq 2]
http public   POST   /mcp -> 202   [seq 3]
http public   POST   /mcp -> 200   [seq 4]
sql  GetRecommendation :one x2
http public   DELETE /mcp -> 204   [seq 5]
```

## TestMCPGetPolicyMatchesREST
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one x3
sql  CreatePool :one
sql  UpsertAggregate :one x30
http public   POST   /mcp -> 200   [seq 1]
http public   POST   /mcp -> 202   [seq 2]
http public   GET    /v1/policies?capability=structured_extraction -> 200   [seq 3]
http public   POST   /mcp -> 200   [seq 4]
http public   GET    /v1/policies -> 200   [seq 5]
http public   POST   /mcp -> 200   [seq 6]
http public   GET    /v1/policies?capability=structured_extraction&pool=default -> 200   [seq 7]
http public   POST   /mcp -> 200   [seq 8]
http public   DELETE /mcp -> 204   [seq 9]
```

## TestMCPListModelsMatchesREST
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one x2
http public   GET    /v1/models -> 200   [seq 1]
http public   POST   /mcp -> 200   [seq 2]
http public   POST   /mcp -> 202   [seq 3]
http public   POST   /mcp -> 200   [seq 4]
http public   DELETE /mcp -> 204   [seq 5]
```

## TestMCPListsExactlyTheSpecTools
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   POST   /mcp -> 200   [seq 1]
http public   POST   /mcp -> 202   [seq 2]
http public   POST   /mcp -> 200   [seq 3]
http public   DELETE /mcp -> 204   [seq 4]
```

## TestMCPEndpointRequiresAuth
```
http public   POST   /mcp -> 401 unauthorized   [seq 1]
http public   POST   /mcp -> 401 unauthorized   [seq 2]
http public   DELETE /mcp -> 401 unauthorized   [seq 3]
http public   POST   /mcp -> 401 unauthorized   [seq 4]
http public   DELETE /mcp -> 401 unauthorized   [seq 5]
```

## TestMCPRateLimitAppliesToMCPRequests
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   POST   /mcp -> 200   [seq 1]
http public   POST   /mcp -> 202   [seq 2]
http public   POST   /mcp -> 429 rate_limited   [seq 3]
http public   DELETE /mcp -> 429 rate_limited   [seq 4]
```

## TestMCPToolCallsResolveTenantPerRequest
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  CreateModelColumn :one
sql  CreatePool :one
http public   POST   /mcp -> 200   [seq 1]
http public   POST   /mcp -> 202   [seq 2]
http public   POST   /mcp -> 200   [seq 3]
sql  GetRecommendation :one
http public   POST   /mcp -> 200   [seq 4]
sql  GetRecommendation :one
http public   DELETE /mcp -> 204   [seq 5]
```

## TestMCPRecommendModelBadArgs
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
sql  CreatePool :one
http public   POST   /mcp -> 200   [seq 1]
http public   POST   /mcp -> 202   [seq 2]
http public   POST   /mcp -> 200   [seq 3]
http public   POST   /mcp -> 200   [seq 4]
http public   POST   /mcp -> 200   [seq 5]
http public   POST   /mcp -> 200   [seq 6]
http public   POST   /mcp -> 200   [seq 7]
http public   POST   /mcp -> 200   [seq 8]
http public   POST   /mcp -> 200   [seq 9]
http public   POST   /mcp -> 200   [seq 10]
sql  ListRecommendationsPage :many
http public   POST   /mcp -> 200   [seq 11]
http public   DELETE /mcp -> 204   [seq 12]
```

## TestMCPStdioStyleTransportBindsRunContextIdentity
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one x3
sql  CreatePool :one
sql  UpsertAggregate :one x24
http public   POST   /v1/recommend -> 200   [seq 1]
sql  GetRecommendation :one
```

## TestModelsRequiresAuth
```
http public   GET    /v1/models -> 401 unauthorized   [seq 1]
```

## TestModelsWrongMethod
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   POST   /v1/models -> 404 not_found   [seq 1]
```

## TestModelsCatalogFieldExact
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one x4
sql  UpsertAggregate :one x2
http public   GET    /v1/models -> 200   [seq 1]
```

## TestModelsEntryKeySetExact
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
sql  UpsertAggregate :one
http public   GET    /v1/models -> 200   [seq 1]
```

## TestModelsCarriesExecutionAliases
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
http public   GET    /v1/models -> 200   [seq 1]
```

## TestPoliciesRequiresAuth
```
http public   GET    /v1/policies -> 401 unauthorized   [seq 1]
```

## TestPoliciesResponseShapeByteExact
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one x3
sql  CreatePool :one
sql  UpsertAggregate :one x30
http public   GET    /v1/policies?capability=structured_extraction -> 200   [seq 1]
```

## TestPoliciesAllCapabilitiesInOrder
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x5
http public   GET    /v1/policies -> 200   [seq 1]
```

## TestPoliciesClearsBandFalseFallback
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one x2
sql  CreatePool :one
sql  UpsertAggregate :one x20
http public   GET    /v1/policies?capability=multi_step_reasoning -> 200   [seq 1]
```

## TestPoliciesQualityBarOverride
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x5
sql  CreateTenantWeight :one
http public   GET    /v1/policies?capability=code_generation -> 200   [seq 1]
```

## TestPoliciesQualitySourceColumn
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one x2
sql  CreatePool :one
sql  UpsertAggregate :one x10
http public   GET    /v1/policies?capability=long_context_qa -> 200   [seq 1]
```

## TestPoliciesExcludesRetiredColumns
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one x2
sql  CreatePool :one
sql  UpsertAggregate :one x20
http public   GET    /v1/policies?capability=factual_qa -> 200   [seq 1]
```

## TestPoliciesNamedPoolSelection
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one x2
sql  CreatePool :one x2
http public   GET    /v1/policies?capability=factual_qa&pool=vertex-only -> 200   [seq 1]
```

## TestPoliciesUnknownCapability
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   GET    /v1/policies?capability=poetry -> 422 invalid_request   [seq 1]
```

## TestPoliciesUnknownQueryParameter
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   GET    /v1/policies?mode=cost -> 422 invalid_request   [seq 1]
```

## TestPoliciesUnknownPool
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
sql  CreatePool :one
http public   GET    /v1/policies?pool=nope -> 422 unknown_pool   [seq 1]
```

## TestPoliciesNoDefaultPool
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   GET    /v1/policies -> 422 unknown_pool   [seq 1]
```

## TestPoliciesCrossTenantIsolation
```
sql  CreateTenant :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
sql  CreatePool :one
http public   GET    /v1/policies?pool=team-a -> 422 unknown_pool   [seq 1]
http public   GET    /v1/policies?capability=factual_qa -> 200   [seq 2]
```

## TestRecommendPromptFormClassified
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
sql  UpsertAggregate :one x8
http public   POST   /v1/recommend -> 200   [seq 1]
sql  GetRecommendation :one
```

## TestRecommendPromptSecondRequestHitsCache
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
sql  CreatePool :one
http public   POST   /v1/recommend -> 200   [seq 1]
http public   POST   /v1/recommend -> 200   [seq 2]
```

## TestRecommendPromptClassifierOutageFallsBack
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
sql  CreatePool :one
http public   POST   /v1/recommend -> 200   [seq 1]
```

## TestRecommendPromptHighStakesTagDerivation/medical_tag
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
sql  CreatePool :one
http public   POST   /v1/recommend -> 200   [seq 1]
```

## TestRecommendPromptHighStakesTagDerivation/non-stakes_tags
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
sql  CreatePool :one
http public   POST   /v1/recommend -> 200   [seq 1]
```

## TestRecommendPromptOverCharLimit422
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
sql  CreatePool :one
http public   POST   /v1/recommend -> 422 invalid_request   [seq 1]
```

## TestRecommendPromptAtCharLimitSurfacesTruncated
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
sql  CreatePool :one
http public   POST   /v1/recommend -> 200   [seq 1]
```

## TestRecommendPromptRejectsDeclaredFields
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   POST   /v1/recommend -> 422 invalid_request   [seq 1]
http public   POST   /v1/recommend -> 422 invalid_request   [seq 2]
```

## TestRecommendRequiresAuth
```
http public   POST   /v1/recommend -> 401 unauthorized   [seq 1]
```

## TestRecommendDeclaredResponseByteExact
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one x3
sql  CreatePool :one
sql  UpsertAggregate :one x24
http public   POST   /v1/recommend -> 200   [seq 1]
```

## TestRecommendDifficultyDefaultsOnColdStart
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one
sql  CreatePool :one
http public   POST   /v1/recommend -> 200   [seq 1]
```

## TestRecommendEntriesCarryExecutionAliases
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
sql  CreatePool :one
http public   POST   /v1/recommend -> 200   [seq 1]
```

## TestRecommendHighStakesRequiresListedTag
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one
sql  CreatePool :one
http public   POST   /v1/recommend -> 200   [seq 1]
http public   POST   /v1/recommend -> 200   [seq 2]
```

## TestRecommendRequestErrors
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one x2
sql  CreatePool :one
http public   POST   /v1/recommend -> 422 invalid_request   [seq 1]
http public   POST   /v1/recommend -> 422 invalid_request   [seq 2]
http public   POST   /v1/recommend -> 422 invalid_request   [seq 3]
http public   POST   /v1/recommend -> 422 invalid_request   [seq 4]
http public   POST   /v1/recommend -> 422 invalid_request   [seq 5]
http public   POST   /v1/recommend -> 422 invalid_request   [seq 6]
http public   POST   /v1/recommend -> 422 invalid_request   [seq 7]
http public   POST   /v1/recommend -> 422 invalid_request   [seq 8]
http public   POST   /v1/recommend -> 422 invalid_request   [seq 9]
http public   POST   /v1/recommend -> 422 invalid_request   [seq 10]
http public   POST   /v1/recommend -> 422 invalid_request   [seq 11]
http public   POST   /v1/recommend -> 422 invalid_request   [seq 12]
http public   POST   /v1/recommend -> 422 invalid_request   [seq 13]
http public   POST   /v1/recommend -> 422 invalid_request   [seq 14]
http public   POST   /v1/recommend -> 422 invalid_request   [seq 15]
http public   POST   /v1/recommend -> 422 invalid_request   [seq 16]
http public   POST   /v1/recommend -> 422 invalid_request   [seq 17]
http public   POST   /v1/recommend -> 422 unknown_pool   [seq 18]
http public   POST   /v1/recommend -> 422 unknown_model   [seq 19]
http public   POST   /v1/recommend -> 422 model_not_in_pool   [seq 20]
```

## TestRecommendTraceRowMixedSurvivorsAndExclusions
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  UpsertCorpusVersion :one
sql  CreateModelColumn :one x5
sql  CreatePool :one
sql  UpsertTenantConstraints :one
sql  UpsertAggregate :one x15
http public   POST   /v1/recommend -> 200   [seq 1]
sql  GetRecommendation :one
```

## TestRecommendPickViolatingPreferenceFlagsResponse
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one x2
sql  CreatePool :one
sql  UpsertAggregate :one x4
http public   POST   /v1/recommend -> 200   [seq 1]
```

## TestRecommendDriftFallbackPersistsAlert
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateModelColumn :one x2
sql  CreatePool :one
sql  UpsertAggregate :one x2
sql  UpsertTenantConstraints :one
http public   POST   /v1/recommend -> 200   [seq 1]
sql  ListAlertsByKind :many
```

## TestChecksRequiresAuth
```
http public   GET    /v1/checks/extraction/v1 -> 401 unauthorized   [seq 1]
```

## TestChecksKnownRefShape
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   GET    /v1/checks/extraction/v1 -> 200   [seq 1]
```

## TestChecksUnknownRef404
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   GET    /v1/checks/poetry/v1 -> 404 not_found   [seq 1]
```

## TestChecksEveryCapabilityRefResolvable
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   GET    /v1/checks/code_generation/v1 -> 200   [seq 1]
http public   GET    /v1/checks/extraction/v1 -> 200   [seq 2]
http public   GET    /v1/checks/summarization/v1 -> 200   [seq 3]
http public   GET    /v1/checks/reasoning/v1 -> 200   [seq 4]
http public   GET    /v1/checks/schema_following/v1 -> 200   [seq 5]
http public   GET    /v1/checks/long_context_qa/v1 -> 200   [seq 6]
http public   GET    /v1/checks/factual_qa/v1 -> 200   [seq 7]
http public   GET    /v1/checks/rewriting/v1 -> 200   [seq 8]
http public   GET    /v1/checks/translation/v1 -> 200   [seq 9]
http public   GET    /v1/checks/critique/v1 -> 200   [seq 10]
```

## TestPublicUnknownPathReturnsEnvelope
```
http public   GET    /nope -> 404 not_found   [seq 1]
```

## TestPublicMethodNotAllowedReturnsEnvelope
```
http public   POST   /up -> 404 not_found   [seq 1]
```

## TestV1MissingKey401
```
http public   GET    /v1/models -> 401 unauthorized   [seq 1]
```

## TestV1MissingKeyEnvelopeByteExact
```
http public   GET    /v1/models -> 401 unauthorized   [seq 1]
```

## TestV1GarbageKey401
```
http public   GET    /v1/models -> 401 unauthorized   [seq 1]
```

## TestV1UnknownKey401
```
http public   GET    /v1/models -> 401 unauthorized   [seq 1]
```

## TestV1RevokedKey401
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  RevokeAPIKey :one
http public   GET    /v1/models -> 401 unauthorized   [seq 1]
```

## TestV1SuspendedTenant403
```
sql  CreateTenant :one
sql  UpdateTenant :one
sql  CreateAPIKey :one
http public   GET    /v1/models -> 403 tenant_suspended   [seq 1]
```

## TestV1AuthedResponseCarriesRateLimitHeaders
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   GET    /v1/models -> 200   [seq 1]
```

## TestV1RateLimitExceeded429
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   GET    /v1/models -> 200   [seq 1]
http public   GET    /v1/models -> 200   [seq 2]
http public   GET    /v1/models -> 429 rate_limited   [seq 3]
```

## TestV1RateLimitIsPerKey
```
sql  CreateTenant :one
sql  CreateAPIKey :one x2
http public   GET    /v1/models -> 200   [seq 1]
http public   GET    /v1/models -> 429 rate_limited   [seq 2]
http public   GET    /v1/models -> 200   [seq 3]
```

## TestV1OversizeBody413
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   POST   /v1/recommend -> 413 payload_too_large   [seq 1]
```

## TestV1BodyAtCapIsNotRejected
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   POST   /v1/recommend -> 422 invalid_request   [seq 1]
```

## TestKeyIssueVerifyRoundTrip
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  GetAPIKeyByHash :one
sql  GetTenant :one
http public   GET    /v1/models -> 200   [seq 1]
```

## TestUpOKWithLiveDatabase
```
http public   GET    /up -> 200   [seq 1]
```

## TestUpReturns503WhenDatabaseUnreachable
```
http public   GET    /up -> 503   [seq 1]
```

## TestInternalHealthDetailRequiresBearer
```
http internal GET    /health/detail -> 401 unauthorized   [seq 1]
http internal GET    /health/detail -> 401 unauthorized   [seq 2]
```

## TestInternalHealthDetailAcceptsEachRotationToken
```
http internal GET    /health/detail -> 200   [seq 1]
http internal GET    /health/detail -> 200   [seq 2]
```

## TestInternalHealthDetailShape
```
http internal GET    /health/detail -> 200   [seq 1]
```

## TestInternalUnknownPathsReturnEnvelope
```
http internal GET    /nope -> 404 not_found   [seq 1]
http internal GET    /internal/v1/tenants/ten_x -> 401 unauthorized   [seq 2]
http internal GET    /internal/v1/tenants/ten_x -> 404 not_found   [seq 3]
```

## TestPublicResponsesCarryRequestID
```
http public   GET    /up -> 200   [seq 1]
```

## TestTelemetryRequiresAuth
```
http public   POST   /v1/telemetry -> 401 unauthorized   [seq 1]
```

## TestOutcomesRouteRemoved
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   POST   /v1/outcomes -> 404 not_found   [seq 1]
```

## TestTelemetryEmptyItemsNoOp
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   POST   /v1/telemetry -> 200   [seq 1]
```

## TestTelemetryEnvelopeInvalidRequest422
```
sql  CreateTenant :one
sql  CreateAPIKey :one
http public   POST   /v1/telemetry -> 422 invalid_request   [seq 1]
http public   POST   /v1/telemetry -> 422 invalid_request   [seq 2]
http public   POST   /v1/telemetry -> 422 invalid_request   [seq 3]
http public   POST   /v1/telemetry -> 422 invalid_request   [seq 4]
http public   POST   /v1/telemetry -> 422 invalid_request   [seq 5]
http public   POST   /v1/telemetry -> 422 invalid_request   [seq 6]
http public   POST   /v1/telemetry -> 422 invalid_request   [seq 7]
http public   POST   /v1/telemetry -> 422 invalid_request   [seq 8]
http public   POST   /v1/telemetry -> 422 invalid_request   [seq 9]
http public   POST   /v1/telemetry -> 422 invalid_request   [seq 10]
```

## TestTelemetryDerivationTable
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateRecommendation :one
http public   POST   /v1/telemetry -> 200   [seq 1]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 2]
sql  CreateRecommendation :one
http public   POST   /v1/telemetry -> 200   [seq 3]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 4]
sql  CreateRecommendation :one
http public   POST   /v1/telemetry -> 200   [seq 5]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 6]
sql  CreateRecommendation :one
http public   POST   /v1/telemetry -> 200   [seq 7]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 8]
sql  CreateRecommendation :one
http public   POST   /v1/telemetry -> 200   [seq 9]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 10]
sql  CreateRecommendation :one
http public   POST   /v1/telemetry -> 200   [seq 11]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 12]
sql  CreateRecommendation :one
http public   POST   /v1/telemetry -> 200   [seq 13]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 14]
sql  CreateRecommendation :one
http public   POST   /v1/telemetry -> 200   [seq 15]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 16]
sql  CreateRecommendation :one
http public   POST   /v1/telemetry -> 200   [seq 17]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 18]
sql  CreateRecommendation :one
http public   POST   /v1/telemetry -> 200   [seq 19]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 20]
sql  CreateRecommendation :one
http public   POST   /v1/telemetry -> 200   [seq 21]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 22]
sql  CreateRecommendation :one
http public   POST   /v1/telemetry -> 200   [seq 23]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 24]
sql  CreateRecommendation :one
http public   POST   /v1/telemetry -> 200   [seq 25]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 26]
sql  CreateRecommendation :one
http public   POST   /v1/telemetry -> 200   [seq 27]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 28]
```

## TestTelemetryAcceptedDroppedCounts
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateRecommendation :one
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateRecommendation :one
http public   POST   /v1/telemetry -> 200   [seq 1]
http public   GET    /v1/history/outcomes -> 200   [seq 2]
sql  CreateAPIKey :one
http public   GET    /v1/history/outcomes -> 200   [seq 3]
```

## TestTelemetryDropsInvalidItems
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateRecommendation :one
http public   POST   /v1/telemetry -> 200   [seq 1]
http public   POST   /v1/telemetry -> 200   [seq 2]
http public   POST   /v1/telemetry -> 200   [seq 3]
http public   POST   /v1/telemetry -> 200   [seq 4]
http public   POST   /v1/telemetry -> 200   [seq 5]
http public   POST   /v1/telemetry -> 200   [seq 6]
http public   POST   /v1/telemetry -> 200   [seq 7]
http public   POST   /v1/telemetry -> 200   [seq 8]
http public   POST   /v1/telemetry -> 200   [seq 9]
http public   POST   /v1/telemetry -> 200   [seq 10]
http public   POST   /v1/telemetry -> 200   [seq 11]
http public   POST   /v1/telemetry -> 200   [seq 12]
http public   POST   /v1/telemetry -> 200   [seq 13]
```

## TestTelemetryEndpointUsedDefaultAndStored
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateRecommendation :one
http public   POST   /v1/telemetry -> 200   [seq 1]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 2]
sql  CreateRecommendation :one
http public   POST   /v1/telemetry -> 200   [seq 3]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 4]
```

## TestTelemetryDetailsContainsExactSuppliedFacts
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateRecommendation :one
http public   POST   /v1/telemetry -> 200   [seq 1]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 2]
sql  CreateRecommendation :one
http public   POST   /v1/telemetry -> 200   [seq 3]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 4]
```

## TestTelemetryMultipleItemsPerRecommendationAppended
```
sql  CreateTenant :one
sql  CreateAPIKey :one
sql  CreateRecommendation :one
http public   POST   /v1/telemetry -> 200   [seq 1]
http public   GET    /v1/history/outcomes?recommendation_id=<rec> -> 200   [seq 2]
```
