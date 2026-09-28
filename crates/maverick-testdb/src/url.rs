//! Connection URL rewriting.

/// Returns `url` with its database path replaced by `name`. The scheme, the credentials,
/// the host, and the query string are kept as they are.
pub(crate) fn with_database(url: &str, name: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("postgres", url));
    let (location, query) = match rest.split_once('?') {
        Some((location, query)) => (location, Some(query)),
        None => (rest, None),
    };
    let authority = location
        .split_once('/')
        .map_or(location, |(authority, _)| authority);
    match query {
        Some(query) => format!("{scheme}://{authority}/{name}?{query}"),
        None => format!("{scheme}://{authority}/{name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::with_database;

    #[test]
    fn replaces_the_database_and_keeps_the_query() {
        assert_eq!(
            with_database(
                "postgres://maverick:maverick@localhost:5433/maverick?sslmode=disable",
                "mvt_x"
            ),
            "postgres://maverick:maverick@localhost:5433/mvt_x?sslmode=disable"
        );
    }

    #[test]
    fn adds_a_database_when_the_url_has_none() {
        assert_eq!(
            with_database("postgresql://u:p@db:5432", "mvt_x"),
            "postgresql://u:p@db:5432/mvt_x"
        );
        assert_eq!(
            with_database("postgres://u@db/?sslmode=require", "mvt_x"),
            "postgres://u@db/mvt_x?sslmode=require"
        );
    }
}
