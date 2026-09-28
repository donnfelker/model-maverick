-- CreateOutcome stores one derived outcome row (integration-surfaces plan
-- §1, G13): written only by the POST /v1/telemetry producer now, never by a
-- caller directly. escalated_to has no telemetry equivalent and is left
-- null by omission (column default).
-- name: CreateOutcome :one
insert into outcomes (id, recommendation_id, model_used, outcome, endpoint_used, details)
values ($1, $2, $3, $4, $5, $6)
returning *;

-- Keyset page of the tenant's reported outcomes (spec §9.6), newest first by
-- ULID id. Tenant scope comes from the owning recommendation, so filtering by
-- another tenant's recommendation_id yields an empty page, never a hint that
-- the id exists.
-- name: ListOutcomesPage :many
select o.* from outcomes o
join recommendations r on r.id = o.recommendation_id
where r.tenant_id = @tenant_id
  and (@recommendation_id::text = '' or o.recommendation_id = @recommendation_id)
  and (@outcome::text = '' or o.outcome = @outcome)
  and (sqlc.narg('since')::timestamptz is null or o.created_at >= sqlc.narg('since'))
  and (@cursor_id::text = '' or o.id < @cursor_id)
order by o.id desc
limit @page_limit;

-- history_prune (spec §10.3): outcomes anchor to their recommendation, so
-- they leave when it does (FK), whatever their own age.
-- name: DeleteOutcomesForRecommendationsBefore :execrows
delete from outcomes o
using recommendations r
where o.recommendation_id = r.id
  and r.created_at < $1;
