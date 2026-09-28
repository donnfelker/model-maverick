-- name: InsertAlert :one
insert into alerts (id, kind, severity, subject, message)
values ($1, $2, $3, $4, $5)
returning *;

-- name: ListAlertsByKind :many
select * from alerts
where kind = $1
order by created_at, id;

-- Regression alerts for one column, matched on the machine-readable subject
-- (spec §3 alerts.subject; §9.6 regression markers). The regression_scan job
-- writes subjects carrying model_column_id/capability/difficulty_tier/as_of.
-- name: ListRegressionAlertsForColumn :many
select * from alerts
where kind = 'regression'
  and subject->>'model_column_id' = @model_column_id::text
order by created_at, id;

-- Regression alerts for one cell — the §6.5 dedup state ("one alert per cell
-- per 7 days unless the drop deepens by ≥2 further points"). difficulty_tier
-- is compared as text so a malformed subject can never fail the scan.
-- name: ListRegressionAlertsForCell :many
select * from alerts
where kind = 'regression'
  and subject->>'model_column_id' = @model_column_id::text
  and subject->>'capability' = @capability::text
  and subject->>'difficulty_tier' = @difficulty_tier::text
order by created_at, id;

-- The §13 operator alert feed (GET /internal/v1/alerts?since=&kind=): every
-- alert row, oldest first, optionally narrowed to one kind and/or a window
-- starting at since.
-- name: ListAlertsFeed :many
select * from alerts
where (@kind::text = '' or kind = @kind)
  and (sqlc.narg('since')::timestamptz is null or created_at >= sqlc.narg('since'))
order by created_at, id;
