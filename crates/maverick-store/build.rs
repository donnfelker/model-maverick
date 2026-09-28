// `sqlx::migrate!` embeds `migrations/` at compile time. Rebuild when a file there changes.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
