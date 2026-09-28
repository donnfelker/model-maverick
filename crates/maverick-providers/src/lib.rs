//! Provider clients and the fake provider.
//!
//! Hand-written typed clients for Anthropic, OpenAI, and Google, with xAI going
//! through the OpenAI client on a different base URL. Holds the deterministic fake
//! provider used in CI (no real provider is ever called in CI) and the retry logic.
//! Package 2c fills this in. The skeleton is intentionally empty.
