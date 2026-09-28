-- Classifier budget counters (spec v1 §3 and §8.3, D4). Postgres holds them so
-- that every API replica shares one count (no Redis). One row per UTC day and
-- tenant.
--
-- Quota: before a classifier call, increment calls with an upsert that only
-- applies while calls is below the tenant's quota. No returned row means the
-- quota is used up and no call is made.
-- Spend cap: the day's global spend is the sum of spend_usd over all tenants.
-- The primary key starts with usage_date, so that sum reads one key range.
-- Cache hits never count.

create table classifier_usage (
  usage_date  date not null,
  tenant_id   text not null references tenants(id),
  calls       int not null default 0,
  spend_usd   numeric(12,6) not null default 0,
  updated_at  timestamptz not null default now(),
  primary key (usage_date, tenant_id)
);
