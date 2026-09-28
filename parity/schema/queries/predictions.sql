-- The predictions ledger is locked BEFORE tonight's batch executes (spec §11
-- prediction_lock at 02:50, strictly before the 03:00 batch), so its
-- batch_id cannot be the runner's batch ULID — that id does not exist until
-- the batch starts. Rows are keyed by the batch *night* (the UTC date, e.g.
-- '2026-07-06') instead: one lock per night, shared by every batch that
-- night (nightly_smoke, plus weekly_sweep on sweep nights).
-- calibration_score joins these rows back to a real batch by night and
-- writes calibration rows under the runner's batch ULID.

-- First lock wins: a retry or late duplicate can never overwrite a locked
-- probability with post-result hindsight (spec §11: "locked before results
-- exist; hindsight impossible").
-- name: InsertPredictionLock :execrows
insert into predictions (
  batch_id, model_column_id, capability, difficulty_tier,
  predicted_pass_prob, source, matrix_version, locked_at
)
values ($1, $2, $3, $4, $5, $6, $7, $8)
on conflict (batch_id, model_column_id, capability, difficulty_tier) do nothing;

-- name: ListPredictionsByNight :many
select * from predictions
where batch_id = $1
order by model_column_id, capability, difficulty_tier;

-- name: CountPredictionsByNight :one
select count(*) from predictions where batch_id = $1;
