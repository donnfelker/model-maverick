-- The master-key ring version that encrypted each capture (spec v1 §10.4,
-- D18). Decryption selects the key by this version, so the key can rotate
-- while older captures stay readable until they expire.

alter table debug_captures
  add column capture_key_version int not null;
