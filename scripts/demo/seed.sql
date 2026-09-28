-- Demo fixture for `maverick demo-run` (plan task 0.5): one corpus version,
-- one task, one model column served by the fake provider. Idempotent so the
-- demo can rerun against the same database.

insert into corpus_versions (version, description, task_count, active)
values ('2026-07-01', 'demo corpus', 1, true)
on conflict (version) do nothing;

insert into tasks (
  id, capability, domain_tags, difficulty_tier, difficulty_knobs, prompt,
  attachments, temperature, max_tokens, grader_specs, dimension_weights,
  pass_threshold, smoke_member, corpus_version, active
)
values (
  'task_demo_001', 'structured_extraction', '{demo}', 2, '{}',
  'Extract the invoice total as JSON.',
  '[]', 0, 256,
  '[{"type": "json_schema", "version": 1, "weight_dimensions": {"schema_validity": 1.0}, "spec": {}}]',
  '{"schema_validity": 1.0}',
  0.7, true, '2026-07-01', true
)
on conflict (id) do nothing;

insert into model_columns (id, provider, model_id, endpoint, settings, status, pricing)
values (
  'col_demo_claude', 'anthropic', 'claude-opus-4-8', 'direct', '{}', 'active',
  '{"input_per_mtok": 3.00, "output_per_mtok": 15.00, "currency": "USD", "as_of": "2026-07-01", "source": "list"}'
)
on conflict (id) do nothing;
