-- Per-model parameter constraints (spec v1 §3 and §8.1, D29). The proxy and
-- the SDK cores normalize a caller value to the nearest value the routed model
-- accepts. Shape: {"<field>": {"allowed": [...], "order": [...]}}, keyed by a
-- surface-neutral field name such as "reasoning_effort". The internal column
-- API validates it. An empty object means no constraints: the two Go mappings
-- apply.

alter table model_columns
  add column param_constraints jsonb not null default '{}';
