-- name: CreateModelColumn :one
-- execution_aliases coalesces to '{}' when the caller passes no value
-- (nil), matching the column's own default (openrouter-execution-spec
-- §2.1): a NULL bound to $11 does not defeat the "not null default '{}'"
-- constraint the way an explicit NULL insert normally would.
insert into model_columns (
  id, provider, model_id, endpoint, settings, status, pricing,
  quality_source_column_id, cold_start_until, deprecation_date, execution_aliases
)
values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, coalesce(sqlc.narg(execution_aliases)::jsonb, '{}'))
returning *;

-- name: GetModelColumn :one
select * from model_columns
where id = $1;

-- name: ListModelColumns :many
select * from model_columns
order by id;

-- name: UpdateModelColumn :one
-- execution_aliases coalesces to the stored value when the caller passes no
-- value (nil): callers that forward every other field but don't know about
-- aliases (e.g. jobs/catalog_sync.go, jobs/price_sync.go) leave them
-- untouched; the §2.2 PUT handler replaces the map whole by passing a
-- concrete (possibly edited) value.
update model_columns
set settings = $2,
    status = $3,
    pricing = $4,
    quality_source_column_id = $5,
    cold_start_until = $6,
    deprecation_date = $7,
    execution_aliases = coalesce($8, execution_aliases),
    updated_at = now()
where id = $1
returning *;

-- name: DeleteModelColumn :exec
delete from model_columns
where id = $1;

-- name: ListModelColumnsByStatuses :many
select * from model_columns
where status = any(@statuses::text[])
order by id;

-- name: ListModelColumnsByIDs :many
select * from model_columns
where id = any(@ids::text[])
order by id;
