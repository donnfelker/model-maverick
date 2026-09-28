-- name: UpsertAggregate :one
insert into aggregates (
  model_column_id, capability, difficulty_tier, dimension,
  ewma, variance, n, measured, matrix_version, as_of
)
values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
on conflict (model_column_id, capability, difficulty_tier, dimension, as_of)
do update set
  ewma = excluded.ewma,
  variance = excluded.variance,
  n = excluded.n,
  measured = excluded.measured,
  matrix_version = excluded.matrix_version
returning *;

-- name: ListAggregateSeriesBefore :many
select * from aggregates
where model_column_id = $1
  and capability = $2
  and difficulty_tier = $3
  and dimension = $4
  and as_of < $5
order by as_of;

-- name: GetLatestAggregate :one
select * from aggregates
where model_column_id = $1
  and capability = $2
  and difficulty_tier = $3
  and dimension = $4
order by as_of desc
limit 1;

-- name: ListLatestAggregatesByColumns :many
select distinct on (model_column_id, capability, difficulty_tier, dimension) *
from aggregates
where model_column_id = any(@column_ids::text[])
  and dimension = any(@dimensions::text[])
order by model_column_id, capability, difficulty_tier, dimension, as_of desc;

-- name: ListLatestAggregatesByDimension :many
select distinct on (model_column_id, capability, difficulty_tier) *
from aggregates
where dimension = $1
order by model_column_id, capability, difficulty_tier, as_of desc;

-- Keyset page of one column's aggregate series for a dimension
-- (spec §9.6 GET /v1/history/models/{column_id}), ascending by
-- (as_of, capability, difficulty_tier) — a time series, oldest first.
-- name: ListAggregateHistoryPage :many
select * from aggregates
where model_column_id = @model_column_id
  and dimension = @dimension
  and (@capability::text = '' or capability = @capability)
  and (sqlc.narg('difficulty')::smallint is null or difficulty_tier = sqlc.narg('difficulty'))
  and (sqlc.narg('since')::date is null or as_of >= sqlc.narg('since'))
  and (@cursor_set::boolean = false
       or (as_of, capability, difficulty_tier) > (@cursor_as_of::date, @cursor_capability::text, @cursor_tier::smallint))
order by as_of, capability, difficulty_tier
limit @page_limit;

-- Every cell that folded a new observation under one as_of date for one
-- dimension — the regression scan's work list for a night (spec §6.5).
-- name: ListAggregatesAtAsOf :many
select * from aggregates
where dimension = $1
  and as_of = $2
order by model_column_id, capability, difficulty_tier;

-- name: ListMeasuredColumnIDs :many
select distinct model_column_id from aggregates
where measured
order by model_column_id;

-- name: ListLatestCellAggregates :many
select distinct on (model_column_id, difficulty_tier, dimension) *
from aggregates
where model_column_id = any(@column_ids::text[])
  and capability = @capability
  and dimension = any(@dimensions::text[])
order by model_column_id, difficulty_tier, dimension, as_of desc;
