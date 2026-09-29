//! The internal audit lines (reference Part 5 §10.2): INFO `internal provisioning write`
//! with `audit=true`, `actor` (the internal token label), `action`, then the action's
//! subject pairs. Handlers call these only after the write succeeded.

use std::fmt;

use serde::Serialize;

/// The first character of a [`JsonField`]'s `Debug` text. The engine's slog layer
/// writes the rest of the text as raw JSON instead of a string, as Go `slog` did for a
/// slice or struct attribute.
pub const JSON_FIELD_MARK: char = '\u{1e}';

/// A structured attribute (a list or an object) that the log line carries as JSON.
pub struct JsonField(String);

impl JsonField {
    /// Go `json.Marshal` text of `value`: struct fields in declaration order, and `<`,
    /// `>`, `&`, U+2028, and U+2029 escaped.
    pub fn of(value: &impl Serialize) -> Self {
        let text = serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned());
        JsonField(escape_like_go(&text))
    }

    pub fn as_json(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for JsonField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{JSON_FIELD_MARK}{}", self.0)
    }
}

/// These characters appear only inside JSON strings, so replacing them in the whole
/// text escapes exactly the string contents.
fn escape_like_go(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        match c {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out
}

/// `tenant.create` (tenant_id, name, default_pool_columns).
pub fn tenant_create(actor: &str, tenant_id: &str, name: &str, default_pool_columns: &[String]) {
    tracing::info!(
        audit = true,
        actor,
        action = "tenant.create",
        tenant_id,
        name,
        default_pool_columns = ?JsonField::of(&default_pool_columns),
        "internal provisioning write"
    );
}

/// `tenant.suspend` (tenant_id, status).
pub fn tenant_suspend(actor: &str, tenant_id: &str, status: &str) {
    tenant_status(actor, "tenant.suspend", tenant_id, status);
}

/// `tenant.activate` (tenant_id, status).
pub fn tenant_activate(actor: &str, tenant_id: &str, status: &str) {
    tenant_status(actor, "tenant.activate", tenant_id, status);
}

fn tenant_status(actor: &str, action: &str, tenant_id: &str, status: &str) {
    tracing::info!(
        audit = true,
        actor,
        action,
        tenant_id,
        status,
        "internal provisioning write"
    );
}

/// `api_key.issue` (tenant_id, api_key_id, name, last4).
pub fn api_key_issue(actor: &str, tenant_id: &str, api_key_id: &str, name: &str, last4: &str) {
    tracing::info!(
        audit = true,
        actor,
        action = "api_key.issue",
        tenant_id,
        api_key_id,
        name,
        last4,
        "internal provisioning write"
    );
}

/// `api_key.revoke` (tenant_id, api_key_id).
pub fn api_key_revoke(actor: &str, tenant_id: &str, api_key_id: &str) {
    tracing::info!(
        audit = true,
        actor,
        action = "api_key.revoke",
        tenant_id,
        api_key_id,
        "internal provisioning write"
    );
}

/// `tenant.pools.replace` (tenant_id, pools as JSON).
pub fn pools_replace(actor: &str, tenant_id: &str, pools: &impl Serialize) {
    tracing::info!(
        audit = true,
        actor,
        action = "tenant.pools.replace",
        tenant_id,
        pools = ?JsonField::of(pools),
        "internal provisioning write"
    );
}

/// `tenant.weights.replace` (tenant_id, weights as JSON).
pub fn weights_replace(actor: &str, tenant_id: &str, weights: &impl Serialize) {
    tracing::info!(
        audit = true,
        actor,
        action = "tenant.weights.replace",
        tenant_id,
        weights = ?JsonField::of(weights),
        "internal provisioning write"
    );
}

/// `tenant.constraints.replace` (tenant_id).
pub fn constraints_replace(actor: &str, tenant_id: &str) {
    tenant_only(actor, "tenant.constraints.replace", tenant_id);
}

/// `tenant.limits.replace` (tenant_id), for the Rust-only limits endpoint.
pub fn limits_replace(actor: &str, tenant_id: &str) {
    tenant_only(actor, "tenant.limits.replace", tenant_id);
}

fn tenant_only(actor: &str, action: &str, tenant_id: &str) {
    tracing::info!(
        audit = true,
        actor,
        action,
        tenant_id,
        "internal provisioning write"
    );
}

/// `column.create` (column_id, provider, model_id, endpoint, status).
pub fn column_create(
    actor: &str,
    column_id: &str,
    provider: &str,
    model_id: &str,
    endpoint: &str,
    status: &str,
) {
    tracing::info!(
        audit = true,
        actor,
        action = "column.create",
        column_id,
        provider,
        model_id,
        endpoint,
        status,
        "internal provisioning write"
    );
}

/// `column.update` (column_id, status).
pub fn column_update(actor: &str, column_id: &str, status: &str) {
    tracing::info!(
        audit = true,
        actor,
        action = "column.update",
        column_id,
        status,
        "internal provisioning write"
    );
}

/// `corpus.activate` (corpus_version).
pub fn corpus_activate(actor: &str, corpus_version: &str) {
    tracing::info!(
        audit = true,
        actor,
        action = "corpus.activate",
        corpus_version,
        "internal provisioning write"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Pool<'a> {
        name: &'a str,
        columns: Vec<&'a str>,
        default: bool,
    }

    #[test]
    fn json_fields_keep_field_order_and_go_escaping() {
        let pools = [Pool {
            name: "a<b>&c",
            columns: vec!["col_1"],
            default: true,
        }];
        let field = JsonField::of(&pools);
        assert_eq!(
            field.as_json(),
            r#"[{"name":"a\u003cb\u003e\u0026c","columns":["col_1"],"default":true}]"#
        );
        assert_eq!(format!("{field:?}"), format!("\u{1e}{}", field.as_json()));
    }

    #[test]
    fn line_separators_are_escaped() {
        assert_eq!(
            JsonField::of(&["x\u{2028}y\u{2029}"]).as_json(),
            r#"["x\u2028y\u2029"]"#
        );
    }
}
