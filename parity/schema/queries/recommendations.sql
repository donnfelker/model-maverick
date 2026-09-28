-- name: CreateRecommendation :one
insert into recommendations (
  id, tenant_id, api_key_id, prompt_hash, request, classification, candidates, response, matrix_version
)
values ($1, $2, $3, $4, $5, $6, $7, $8, $9)
returning *;

-- name: GetRecommendation :one
select * from recommendations
where id = $1;

-- Tenant-scoped read (spec §9.5, §9.6): a recommendation belonging to
-- another tenant is indistinguishable from a nonexistent one — both are
-- pgx.ErrNoRows here.
-- name: GetRecommendationForTenant :one
select * from recommendations
where id = $1 and tenant_id = $2;

-- Bulk tenant-scoped ownership check for a POST /v1/telemetry batch
-- (integration-surfaces plan §1): one round trip resolves which of the
-- batch's recommendation_id values belong to this tenant. An id absent from
-- the result is dropped by the caller — indistinguishable whether it never
-- existed or belongs to another tenant (no cross-tenant existence oracle,
-- same rule as the retired POST /v1/outcomes, spec §9.5).
-- name: RecommendationIDsForTenant :many
select id from recommendations
where tenant_id = @tenant_id and id = any(@ids::text[]);

-- Keyset page of the tenant audit log (spec §9.6), newest first by ULID id
-- (ids are minted monotonically, so id order is creation order and pages stay
-- stable while newer rows arrive).
-- name: ListRecommendationsPage :many
select * from recommendations
where tenant_id = @tenant_id
  and (@prompt_hash::text = '' or prompt_hash = @prompt_hash)
  and (@capability::text = '' or classification->>'capability' = @capability)
  and (sqlc.narg('since')::timestamptz is null or created_at >= sqlc.narg('since'))
  and (@cursor_id::text = '' or id < @cursor_id)
order by id desc
limit @page_limit;

-- history_prune (spec §10.3): recommendations (and their traces, stored
-- inline) past history.retention_days are hard-deleted daily.
-- name: DeleteRecommendationsBefore :execrows
delete from recommendations
where created_at < $1;
