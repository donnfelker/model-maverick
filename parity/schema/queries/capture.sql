-- Debug capture (spec §9.8, §10.4): capture windows, encrypted captures, and
-- the access log that survives capture deletion.

-- name: CreateCaptureWindow :one
insert into capture_windows (id, tenant_id, api_key_id, confidence_below, starts_at, ends_at)
values ($1, $2, $3, $4, $5, $6)
returning *;

-- Tenant-scoped delete (spec §9.8 "end early"): another tenant's window is
-- indistinguishable from a nonexistent one — both delete zero rows.
-- name: DeleteCaptureWindowForTenant :execrows
delete from capture_windows
where id = $1 and tenant_id = $2;

-- Windows live at @at (spec §10.4 request-time capture decision).
-- name: ListActiveCaptureWindows :many
select * from capture_windows
where tenant_id = $1
  and starts_at <= @at
  and ends_at > @at;

-- name: CreateDebugCapture :one
insert into debug_captures (id, tenant_id, window_id, recommendation_id, ciphertext, nonce, expires_at)
values ($1, $2, $3, $4, $5, $6, $7)
returning *;

-- name: GetDebugCaptureForTenant :one
select * from debug_captures
where id = $1 and tenant_id = $2;

-- Operator read (spec §13 internal capture-read surface): no tenant scope —
-- the internal listener is root — so this is the raw lookup the handler
-- layers the 404/410 distinction on top of, same as GetDebugCaptureForTenant
-- does for the tenant-scoped replay path.
-- name: GetDebugCaptureByID :one
select * from debug_captures
where id = $1;

-- name: ListDebugCapturesByTenant :many
select * from debug_captures
where tenant_id = $1
order by id;

-- Operator list (spec §13: GET /internal/v1/captures) — every capture across
-- tenants, newest first, optionally scoped to one window or tenant. Joins
-- recommendations for prompt_hash and the stored original classification;
-- never selects ciphertext/nonce, so no prompt-bearing column can leak here
-- even by future accident.
-- name: ListDebugCapturesPage :many
select d.id, d.tenant_id, d.window_id, d.recommendation_id, d.captured_at, d.expires_at,
       r.prompt_hash, r.classification
from debug_captures d
join recommendations r on r.id = d.recommendation_id
where (@window_id::text = '' or d.window_id = @window_id)
  and (@tenant_id::text = '' or d.tenant_id = @tenant_id)
  and (@cursor_id::text = '' or d.id < @cursor_id)
order by d.id desc
limit @page_limit;

-- Tenant self-service list (spec §9.8 extension: GET /v1/debug/captures) —
-- the caller's own captures only. Unlike the operator page above, the
-- tenant scope is UNCONDITIONAL, not a filter: no parameter value widens
-- this query beyond one tenant. Same no-ciphertext rule: prompt-bearing
-- columns are never selected.
-- name: ListDebugCapturesPageForTenant :many
select d.id, d.window_id, d.recommendation_id, d.captured_at, d.expires_at,
       r.prompt_hash, r.classification
from debug_captures d
join recommendations r on r.id = d.recommendation_id
where d.tenant_id = @tenant_id
  and (@window_id::text = '' or d.window_id = @window_id)
  and (@cursor_id::text = '' or d.id < @cursor_id)
order by d.id desc
limit @page_limit;

-- Hourly hard-delete (spec §10.4/§11 capture_expiry). Returns the deleted
-- rows' identities so the job can append the 'delete' access-log rows.
-- name: DeleteExpiredDebugCaptures :many
delete from debug_captures
where expires_at <= $1
returning id, tenant_id;

-- Windows whose captures can no longer exist (ends_at + capture.retention
-- already passed) are dead weight; capture_expiry sweeps them with the same
-- cutoff arithmetic done by the caller.
-- name: DeleteExpiredCaptureWindows :execrows
delete from capture_windows
where ends_at <= $1;

-- history_prune (spec §10.3): captures still referencing about-to-be-pruned
-- recommendations go first (FK), each one access-logged by the job.
-- name: DeleteCapturesForRecommendationsBefore :many
delete from debug_captures d
using recommendations r
where d.recommendation_id = r.id
  and r.created_at < $1
returning d.id, d.tenant_id;

-- name: InsertCaptureAccessLog :one
insert into capture_access_log (id, capture_id, tenant_id, actor, action)
values ($1, $2, $3, $4, $5)
returning *;

-- name: ListCaptureAccessLogByCapture :many
select * from capture_access_log
where capture_id = $1 and tenant_id = $2
order by id;

-- Distinguishes a hard-deleted capture (410 capture_expired) from one that
-- never existed (404): every hard-delete appends a 'delete' log row, and log
-- rows survive the deletion (spec §10.4, Appendix B).
-- name: CaptureWasDeleted :one
select exists (
  select 1 from capture_access_log
  where capture_id = $1 and tenant_id = $2 and action = 'delete'
) as deleted;

-- Same distinction for the operator read path, which has no tenant to scope
-- by (root, spec §13): the surviving 'delete' row also carries the tenant_id
-- the capture belonged to, so a 410 can still be access-logged correctly.
-- name: FindCaptureDeleteLog :one
select * from capture_access_log
where capture_id = $1 and action = 'delete'
limit 1;
