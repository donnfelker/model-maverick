-- name: InsertScore :one
insert into scores (run_id, grader_idx, dimension, value, detail)
values ($1, $2, $3, $4, $5)
returning *;

-- name: ListScoresByRun :many
select * from scores
where run_id = $1
order by grader_idx, dimension;

-- name: ListScoresByBatch :many
select scores.* from scores
join runs on runs.id = scores.run_id
where runs.batch_id = $1
order by scores.run_id, scores.grader_idx, scores.dimension;
