//! `/`, `/docs` and `/openapi.json`.
//!
//! The legacy Python API redirected `/` to its Swagger page at `/docs`, so people
//! opening api.karlsencoin.org in a browser landed on the documentation. These
//! routes restore that: `/` redirects to `/docs`, which renders `openapi.json`
//! (a static description of the public endpoints) with Swagger UI.

use std::sync::LazyLock;

use axum::{
    http::header,
    response::{Html, IntoResponse, Redirect, Response},
};

/// Static spec; `info.version` is filled from the crate version at first use.
const OPENAPI_TEMPLATE: &str = include_str!("../openapi.json");

static OPENAPI: LazyLock<String> = LazyLock::new(|| {
    let mut spec: serde_json::Value =
        serde_json::from_str(OPENAPI_TEMPLATE).expect("openapi.json is valid JSON");
    spec["info"]["version"] = env!("CARGO_PKG_VERSION").into();
    spec.to_string()
});

/// Swagger UI from jsDelivr, pinned to one major version.
const DOCS_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Karlsen REST API</title>
<link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/swagger-ui-dist@5/swagger-ui.css">
<style>body{margin:0;background:#fafafa}</style>
</head>
<body>
<div id="swagger-ui"></div>
<script src="https://cdn.jsdelivr.net/npm/swagger-ui-dist@5/swagger-ui-bundle.js"></script>
<script>
window.ui = SwaggerUIBundle({
  url: "/openapi.json",
  dom_id: "#swagger-ui",
  deepLinking: true,
  tryItOutEnabled: true
});
</script>
</body>
</html>
"##;

/// `GET /` → `/docs`, as the legacy API did.
pub async fn root() -> Redirect {
    Redirect::temporary("/docs")
}

/// `GET /docs`
pub async fn docs() -> Html<&'static str> {
    Html(DOCS_HTML)
}

/// `GET /openapi.json`
pub async fn openapi() -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        OPENAPI.as_str(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_parses_and_carries_crate_version() {
        let v: serde_json::Value = serde_json::from_str(&OPENAPI).unwrap();
        assert_eq!(v["info"]["version"], env!("CARGO_PKG_VERSION"));
        assert!(v["paths"]["/info/blockdag"]["get"].is_object());
        // Admin endpoints stay out of the public docs.
        assert!(v["paths"]
            .as_object()
            .unwrap()
            .keys()
            .all(|k| !k.starts_with("/admin")));
    }
}
