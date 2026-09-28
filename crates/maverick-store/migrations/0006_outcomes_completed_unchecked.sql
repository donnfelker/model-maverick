-- Widen the outcome set with 'completed_unchecked' (spec v1 §9.10, D16): a 2xx
-- response on which no check ran. It is availability evidence only, never
-- quality evidence. The constraint keeps its baseline name.

alter table outcomes
  drop constraint outcomes_outcome_check,
  add constraint outcomes_outcome_check
    check (outcome in ('success','parse_failure','quality_insufficient',
                       'latency_exceeded','provider_error',
                       'completed_unchecked','other'));
