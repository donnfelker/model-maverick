-- +goose Up

-- execution_aliases: a flat string->string map of execution-backend model
-- slugs (openrouter-execution-spec §2.1). This spec defines one key,
-- "openrouter", whose value is the OpenRouter model slug (e.g.
-- "anthropic/claude-sonnet-4-6"). Unknown keys are preserved and ignored.
-- Metadata only: the column identity index (provider, model_id, endpoint,
-- settings) on model_columns is unchanged.
alter table model_columns
  add column execution_aliases jsonb not null default '{}';

-- endpoint (see 0001_init.sql) gains 'openrouter' as a legal value:
-- 'direct' | 'vertex' | 'bedrock' | 'azure' | 'openrouter'. No such columns
-- are created by this migration; validation is enforced in the internal API
-- layer (spec §2.2), same as the other endpoint values (endpoint carries no
-- database check constraint).

-- +goose Down

alter table model_columns
  drop column execution_aliases;
