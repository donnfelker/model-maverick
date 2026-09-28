-- Retention status per column and per execution alias (spec v1 §4.5, D1, D15;
-- openrouter execution spec §2.1). Only 'zero' with an unexpired
-- retention_valid_until satisfies a zero-retention tenant. Expired evidence
-- reads as 'unknown'. execution_alias_retention is keyed like
-- execution_aliases; each value is {"retention_mode", "retention_verified_at",
-- "retention_valid_until", "retention_source"}.

alter table model_columns
  add column retention_mode text not null default 'unknown'
    check (retention_mode in ('unknown','standard','zero')),
  add column retention_verified_at timestamptz,
  add column retention_valid_until timestamptz,
  add column retention_source text,
  add column execution_alias_retention jsonb not null default '{}';
