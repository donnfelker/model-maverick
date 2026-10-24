-- Conversation affinity (spec v1 §3, §7.6, §10.2, D44). The engine keeps the last
-- pick per conversation. The row is keyed by the per-tenant HMAC of the caller's
-- affinity key, never the key itself. A row is live while expires_at is in the
-- future; the hourly affinity_prune job deletes the others.

create table conversation_affinity (
  tenant_id          text not null references tenants(id),
  affinity_hash      text not null,
  column_id          text not null references model_columns(id),
  recommendation_id  text not null,
  last_used_at       timestamptz not null,
  expires_at         timestamptz not null,
  primary key (tenant_id, affinity_hash)
);
create index conversation_affinity_expires_at on conversation_affinity (expires_at);

-- Cost-aware affinity is a tenant setting, off by default. A request can override it.
alter table tenant_constraints
  add column affinity_cost_aware boolean not null default false;

-- The affinity block of the decision trace. Null when the request sent no affinity.
alter table recommendations
  add column affinity jsonb;
