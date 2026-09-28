-- +goose Up

-- endpoint_used records which execution backend actually served the model
-- (openrouter-execution-spec §2.4): 'direct' | 'vertex' | 'bedrock' |
-- 'azure' | 'openrouter'. Written by the POST /v1/telemetry producer
-- (integration-surfaces plan §1, G13) that replaces POST /v1/outcomes;
-- default 'direct' backfills historic rows, all of which were reported
-- against a direct-endpoint recommendation. No database check constraint —
-- same convention as model_columns.endpoint (see 0001_init.sql, 0002).
alter table outcomes
  add column endpoint_used text not null default 'direct';

-- +goose Down

alter table outcomes
  drop column endpoint_used;
