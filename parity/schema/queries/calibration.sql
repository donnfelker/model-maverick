-- Idempotent per (batch, capability): a calibration_score retry replaces its
-- own row rather than tripping the primary key.
-- name: UpsertCalibration :one
insert into calibration (batch_id, capability, brier_score, overconfidence, n)
values ($1, $2, $3, $4, $5)
on conflict (batch_id, capability) do update set
  brier_score = excluded.brier_score,
  overconfidence = excluded.overconfidence,
  n = excluded.n,
  scored_at = now()
returning *;

-- Keyset page of the calibration ledger (spec §9.6), newest batch first
-- (batch ids are ULIDs, so id order is batch order), capabilities in
-- alphabetical order within a batch.
-- name: ListCalibrationPage :many
select * from calibration
where (@capability::text = '' or capability = @capability)
  and (sqlc.narg('since')::timestamptz is null or scored_at >= sqlc.narg('since'))
  and (@cursor_batch::text = ''
       or batch_id < @cursor_batch
       or (batch_id = @cursor_batch and capability > @cursor_capability::text))
order by batch_id desc, capability
limit @page_limit;
