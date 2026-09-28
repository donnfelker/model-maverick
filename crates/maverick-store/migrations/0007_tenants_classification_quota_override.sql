-- Per-tenant override of the daily classifier quota (spec v1 §8.3 and §13,
-- D4). Set through PUT /internal/v1/tenants/{id}/limits. Null means the config
-- default classifier.daily_quota applies.

alter table tenants
  add column classification_quota_override int;
