-- name: InsertRun :one
insert into runs (
  id, batch_id, task_id, model_column_id, repeat_index, output, tokens_in,
  tokens_out, latency_ms, cost, temperature, status, error, matrix_version
)
values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
returning *;

-- name: GetRun :one
select * from runs
where id = $1;

-- name: ListRunsByBatch :many
select * from runs
where batch_id = $1
order by task_id, model_column_id, repeat_index;

-- name: SumRunCostBetween :one
select coalesce(sum(cost), 0)::numeric(14,6) as total from runs
where ran_at >= @start_at and ran_at < @end_at;

-- Per-column parse-failure counts for one batch (spec §11 parse-failure
-- watch: the batch rate is parse_failures / total).
-- name: ListBatchParseFailureRates :many
select model_column_id,
       count(*) as total,
       count(*) filter (where status = 'parse_failure') as parse_failures
from runs
where batch_id = $1
group by model_column_id
order by model_column_id;

-- Per-column parse-failure counts over a time window — the trailing 7-day
-- baseline the §11 parse-failure watch compares a batch against.
-- name: ListParseFailureRatesBetween :many
select model_column_id,
       count(*) as total,
       count(*) filter (where status = 'parse_failure') as parse_failures
from runs
where ran_at >= @start_at and ran_at < @end_at
group by model_column_id
order by model_column_id;
