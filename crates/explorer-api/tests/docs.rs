//! `/`, `/docs` and `/openapi.json` need neither the database nor the node.

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use karlsen_explorer_api::{routes, AppState};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

async fn get(path: &str) -> (u16, Option<String>, String) {
    let db = PgPoolOptions::new()
        .connect_lazy("postgres://unused@127.0.0.1:1/unused")
        .unwrap();
    let app = routes::router(AppState::new(db, None));
    let resp = app
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let location = resp
        .headers()
        .get("location")
        .map(|v| v.to_str().unwrap().to_string());
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, location, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn root_redirects_to_docs() {
    let (status, location, _) = get("/").await;
    assert_eq!(status, 307);
    assert_eq!(location.as_deref(), Some("/docs"));
}

#[tokio::test]
async fn docs_page_loads_spec() {
    let (status, _, body) = get("/docs").await;
    assert_eq!(status, 200);
    assert!(body.contains("/openapi.json"));
}

#[tokio::test]
async fn openapi_is_valid_json() {
    let (status, _, body) = get("/openapi.json").await;
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["info"]["title"], "Karlsen REST API");
    assert!(v["paths"]["/transactions/search"]["post"].is_object());
}
