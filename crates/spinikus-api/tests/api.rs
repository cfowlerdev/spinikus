//! The API over HTTP requests, against a real service layer and database.

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use spinikus_control::Control;
use spinikus_core::secret::Secret;
use spinikus_store::{MasterKey, SqliteStore, connect, migrate};
use tower::ServiceExt;

mod schema;

const PASSWORD: &str = "correct horse battery staple";

struct Api {
    _dir: tempfile::TempDir,
    control: Control,
    router: Router,
}

struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Value,
}

impl Reply {
    fn header(&self, name: header::HeaderName) -> &str {
        self.headers.get(name).unwrap().to_str().unwrap()
    }
}

impl Api {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let pool = connect(&dir.path().join("spinikus.db")).await.unwrap();
        migrate(&pool).await.unwrap();
        let store = SqliteStore::open(pool, &MasterKey::generate()).await.unwrap();
        // Storage destinations are reached in memory, never over the network.
        let control = Control::new(store).with_backup_store(std::sync::Arc::new(object_store::memory::InMemory::new()));
        let router = spinikus_api::router(control.clone());
        Self { _dir: dir, control, router }
    }

    /// An instance admin's bearer token.
    async fn admin_token(&self) -> String {
        self.control.create_first_admin("admin@example.com", "Admin", Secret::new(PASSWORD)).await.unwrap();
        let reply = self
            .send(Method::POST, "/api/v1/login", None, &[], Some(json!({ "email": "admin@example.com", "password": PASSWORD })))
            .await;
        reply.body["token"].as_str().unwrap().to_string()
    }

    /// A GET whose body is text, such as server-sent events, read to the end.
    async fn get_text(&self, uri: &str, token: &str) -> (StatusCode, String, String) {
        let request = Request::builder().uri(uri).header(header::AUTHORIZATION, format!("Bearer {token}"));
        let response = self.router.clone().oneshot(request.body(Body::empty()).unwrap()).await.unwrap();
        let (parts, body) = response.into_parts();
        let content_type = parts.headers.get(header::CONTENT_TYPE).map(|value| value.to_str().unwrap().to_string());
        let content_type = content_type.unwrap_or_default();
        let bytes = body.collect().await.unwrap().to_bytes();
        let body = if content_type.contains("json") { serde_json::from_slice(&bytes).unwrap() } else { Value::Null };
        schema::check_response("GET", uri, parts.status.as_u16(), &content_type, &body);
        (parts.status, content_type, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn send(
        &self,
        method: Method,
        uri: &str,
        token: Option<&str>,
        headers: &[(header::HeaderName, &str)],
        body: Option<Value>,
    ) -> Reply {
        let mut request = Request::builder().method(method.clone()).uri(uri);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        for (name, value) in headers {
            request = request.header(name, *value);
        }
        let request = match body {
            Some(body) => request.header(header::CONTENT_TYPE, "application/json").body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        };
        let response = self.router.clone().oneshot(request.unwrap()).await.unwrap();
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        let body = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
        let content_type = parts.headers.get(header::CONTENT_TYPE).map_or("", |value| value.to_str().unwrap());
        // Every answer is what the OpenAPI document says it is.
        schema::check_response(method.as_str(), uri, parts.status.as_u16(), content_type, &body);
        Reply { status: parts.status, headers: parts.headers, body }
    }
}

#[tokio::test]
async fn sets_up_the_first_admin_once() {
    let api = Api::new().await;
    let admin = json!({ "email": "ada@example.com", "name": "Ada", "password": PASSWORD });

    let before = api.send(Method::GET, "/api/v1/setup", None, &[], None).await;
    let created = api.send(Method::POST, "/api/v1/setup", None, &[], Some(admin.clone())).await;
    let again = api.send(Method::POST, "/api/v1/setup", None, &[], Some(admin)).await;

    assert_eq!(before.body, json!({ "needs_setup": true, "setup_code_required": false }));
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(created.body["email"], "ada@example.com");
    assert!(created.body.get("password").is_none());
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.header(header::CONTENT_TYPE), "application/problem+json");
    assert_eq!(again.body["code"], "already_set_up");
    assert_eq!(again.body["status"], 409);
}

#[tokio::test]
async fn setting_up_needs_the_setup_code_when_there_is_one() {
    let mut api = Api::new().await;
    let code = Control::new_setup_code();
    api.router = spinikus_api::router(api.control.clone().with_setup_code(Secret::new(code.expose())));
    let admin = |code: &str| json!({ "setup_code": code, "email": "ada@example.com", "name": "Ada", "password": PASSWORD });

    let status = api.send(Method::GET, "/api/v1/setup", None, &[], None).await;
    let wrong = api.send(Method::POST, "/api/v1/setup", None, &[], Some(admin("nope"))).await;
    let created = api.send(Method::POST, "/api/v1/setup", None, &[], Some(admin(code.expose()))).await;

    assert_eq!(status.body, json!({ "needs_setup": true, "setup_code_required": true }));
    assert_eq!(
        (wrong.status, wrong.body["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid")),
        "{:?}",
        wrong.body
    );
    assert_eq!(created.status, StatusCode::CREATED);
}

#[tokio::test]
async fn logs_in_for_a_bearer_token() {
    let api = Api::new().await;
    let token = api.admin_token().await;
    let wrong = json!({ "email": "admin@example.com", "password": "not the password" });

    let failed = api.send(Method::POST, "/api/v1/login", None, &[], Some(wrong.clone())).await;
    let me = api.send(Method::GET, "/api/v1/me", Some(&token), &[], None).await;
    let anonymous = api.send(Method::GET, "/api/v1/me", None, &[], None).await;
    let bogus = api.send(Method::GET, "/api/v1/me", Some("spk_bogus"), &[], None).await;

    assert!(token.starts_with("spk_"));
    assert_eq!((failed.status, &failed.body["code"]), (StatusCode::UNAUTHORIZED, &json!("invalid_credentials")));
    assert_eq!(me.body["email"], "admin@example.com");
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    assert_eq!(anonymous.header(header::WWW_AUTHENTICATE), "Bearer");
    assert_eq!(bogus.body["code"], "unauthenticated");

    // One wrong password so far; after the rest, even the right one waits.
    for _ in 1..spinikus_control::throttle::MAX_FAILURES {
        api.send(Method::POST, "/api/v1/login", None, &[], Some(wrong.clone())).await;
    }
    let right = json!({ "email": "admin@example.com", "password": PASSWORD });
    let throttled = api.send(Method::POST, "/api/v1/login", None, &[], Some(right)).await;
    assert_eq!((throttled.status, &throttled.body["code"]), (StatusCode::TOO_MANY_REQUESTS, &json!("too_many_attempts")));
    let retry_after: u64 = throttled.header(header::RETRY_AFTER).parse().unwrap();
    assert!((890..=900).contains(&retry_after), "{retry_after}");
}

#[tokio::test]
async fn manages_projects_with_revisions() {
    let api = Api::new().await;
    let token = api.admin_token().await;
    let token = Some(token.as_str());

    let created = api.send(Method::POST, "/api/v1/projects", token, &[], Some(json!({ "name": "Web Shop" }))).await;
    let id = created.body["id"].as_str().unwrap().to_string();
    let path = format!("/api/v1/projects/{id}");
    let shown = api.send(Method::GET, &path, token, &[], None).await;
    let rename = json!({ "name": "Shop" });
    let no_revision = api.send(Method::PATCH, &path, token, &[], Some(rename.clone())).await;
    let stale = api.send(Method::PATCH, &path, token, &[(header::IF_MATCH, "\"7\"")], Some(rename.clone())).await;
    let updated = api.send(Method::PATCH, &path, token, &[(header::IF_MATCH, "\"1\"")], Some(rename)).await;

    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(created.header(header::LOCATION), path);
    assert_eq!(created.body["slug"], "web-shop");
    assert_eq!(shown.header(header::ETAG), "\"1\"");
    assert_eq!(
        (no_revision.status, &no_revision.body["code"]),
        (StatusCode::PRECONDITION_REQUIRED, &json!("precondition_required"))
    );
    assert_eq!((stale.status, &stale.body["code"]), (StatusCode::PRECONDITION_FAILED, &json!("revision_mismatch")));
    assert_eq!((updated.status, updated.header(header::ETAG)), (StatusCode::OK, "\"2\""));
    assert_eq!((&updated.body["name"], &updated.body["slug"]), (&json!("Shop"), &json!("web-shop")));

    api.send(Method::POST, "/api/v1/projects", token, &[], Some(json!({ "name": "Blog" }))).await;
    let first = api.send(Method::GET, "/api/v1/projects?limit=1", token, &[], None).await;
    let cursor = first.body["next_cursor"].as_str().unwrap();
    let second = api.send(Method::GET, &format!("/api/v1/projects?limit=1&after={cursor}"), token, &[], None).await;

    assert_eq!(first.body["items"][0]["id"], id.as_str());
    assert_eq!(second.body["items"][0]["slug"], "blog");
    assert_eq!(second.body["next_cursor"], Value::Null);

    let deleted = api.send(Method::DELETE, &path, token, &[], None).await;
    let gone = api.send(Method::GET, &path, token, &[], None).await;

    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_eq!((gone.status, &gone.body["code"]), (StatusCode::NOT_FOUND, &json!("not_found")));
}

#[tokio::test]
async fn creates_environments_and_services() {
    let api = Api::new().await;
    let token = api.admin_token().await;
    let token = Some(token.as_str());
    let project = api.send(Method::POST, "/api/v1/projects", token, &[], Some(json!({ "name": "Shop" }))).await;
    let environments = format!("/api/v1/projects/{}/environments", project.body["id"].as_str().unwrap());

    let no_pr = api.send(Method::POST, &environments, token, &[], Some(json!({ "name": "PR", "kind": "preview" }))).await;
    let production =
        api.send(Method::POST, &environments, token, &[], Some(json!({ "name": "Production", "kind": "production" }))).await;
    let staging = json!({ "name": "Staging", "kind": "staging", "previews": true });
    let staging = api.send(Method::POST, &environments, token, &[], Some(staging)).await;
    let staging_url = format!("/api/v1/environments/{}", staging.body["id"].as_str().unwrap());
    let off =
        api.send(Method::PATCH, &staging_url, token, &[(header::IF_MATCH, "\"1\"")], Some(json!({ "previews": false }))).await;
    let services = format!("/api/v1/environments/{}/services", production.body["id"].as_str().unwrap());
    let typo = api.send(Method::POST, &services, token, &[], Some(json!({ "name": "Web", "kind": "app", "conifg": {} }))).await;
    let web = json!({ "name": "Web", "kind": "app", "config": { "port": 8080 } });
    let created = api.send(Method::POST, &services, token, &[], Some(web)).await;
    let listed = api.send(Method::GET, &services, token, &[], None).await;

    assert_eq!((no_pr.status, &no_pr.body["field"]), (StatusCode::UNPROCESSABLE_ENTITY, &json!("kind")));
    assert_eq!(production.body["kind"], "production");
    assert_eq!((&production.body["previews"], &production.body["base_environment_id"]), (&json!(false), &json!(null)));
    assert_eq!((staging.body["previews"].clone(), off.body["previews"].clone()), (json!(true), json!(false)));
    assert_eq!((typo.status, &typo.body["code"]), (StatusCode::UNPROCESSABLE_ENTITY, &json!("invalid_body")));
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(created.body["config"], json!({ "port": 8080 }));
    assert_eq!(listed.body["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn serves_its_openapi_document_without_a_token() {
    let api = Api::new().await;
    let reply = api.send(Method::GET, "/api/v1/openapi.json", None, &[], None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header(header::CONTENT_TYPE), "application/json");
    assert_eq!(reply.body["openapi"], "3.1.0");
    assert_eq!(reply.body["info"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(reply.body["paths"]["/api/v1/projects"]["post"].is_object());
}

#[tokio::test]
async fn malformed_requests_are_problems_too() {
    let api = Api::new().await;
    let bearer = api.admin_token().await;
    let token = Some(bearer.as_str());

    let bad_id = api.send(Method::GET, "/api/v1/projects/svc_nope", token, &[], None).await;
    let no_route = api.send(Method::GET, "/api/v1/nothing", token, &[], None).await;
    let wrong_method = api.send(Method::PUT, "/api/v1/projects", token, &[], None).await;
    let bad_query = api.send(Method::GET, "/api/v1/projects?limit=lots", token, &[], None).await;
    let bad_if_match = api
        .send(
            Method::PATCH,
            "/api/v1/projects/prj_01J0000000000000000000000A",
            token,
            &[(header::IF_MATCH, "*")],
            Some(json!({})),
        )
        .await;
    let request = Request::post("/api/v1/projects")
        .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    let not_json = api.router.clone().oneshot(request).await.unwrap();

    assert_eq!((bad_id.status, &bad_id.body["code"]), (StatusCode::NOT_FOUND, &json!("not_found")));
    assert_eq!((no_route.status, &no_route.body["code"]), (StatusCode::NOT_FOUND, &json!("not_found")));
    assert_eq!((wrong_method.status, &wrong_method.body["code"]), (StatusCode::METHOD_NOT_ALLOWED, &json!("method_not_allowed")));
    assert_eq!((bad_query.status, &bad_query.body["code"]), (StatusCode::BAD_REQUEST, &json!("invalid_query")));
    assert_eq!((bad_if_match.status, &bad_if_match.body["code"]), (StatusCode::BAD_REQUEST, &json!("invalid_header")));
    assert_eq!(not_json.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn admins_create_servers_and_join_tokens() {
    let api = Api::new().await;
    let bearer = api.admin_token().await;
    let token = Some(bearer.as_str());

    let created = api.send(Method::POST, "/api/v1/servers", token, &[], Some(json!({ "name": "web-1" }))).await;
    let id = created.body["id"].as_str().unwrap().to_string();
    let join = api.send(Method::POST, &format!("/api/v1/servers/{id}/enrollment-tokens"), token, &[], None).await;
    let revoked = api.send(Method::POST, &format!("/api/v1/servers/{id}/revoke"), token, &[], None).await;
    let shown = api.send(Method::GET, &format!("/api/v1/servers/{id}"), token, &[], None).await;
    let listed = api.send(Method::GET, "/api/v1/servers", token, &[], None).await;

    assert_eq!((created.status, &created.body["status"]), (StatusCode::CREATED, &json!("pending")));
    assert_eq!(join.status, StatusCode::CREATED);
    assert!(join.body["token"].as_str().unwrap().starts_with("spj_"));
    assert!(join.body["ca_fingerprint"].as_str().unwrap().starts_with("sha256:"));
    assert_eq!(join.body["enrollment_token"]["server_id"], id.as_str());
    assert_eq!(revoked.status, StatusCode::NO_CONTENT);
    assert_eq!(shown.body["status"], "revoked");
    assert_eq!(listed.body["items"], json!([]), "revoked servers are not listed");
}

#[tokio::test]
async fn users_create_list_and_revoke_their_own_tokens() {
    let api = Api::new().await;
    let bearer = api.admin_token().await;
    let token = Some(bearer.as_str());

    let created = api.send(Method::POST, "/api/v1/tokens", token, &[], Some(json!({ "name": "CI", "role": "developer" }))).await;
    let secret = created.body["token"].as_str().unwrap().to_string();
    let id = created.body["api_token"]["id"].as_str().unwrap().to_string();
    let used = api.send(Method::GET, "/api/v1/me", Some(&secret), &[], None).await;
    let listed = api.send(Method::GET, "/api/v1/tokens", token, &[], None).await;
    let wider =
        api.send(Method::POST, "/api/v1/tokens", Some(&secret), &[], Some(json!({ "name": "Wider", "role": "admin" }))).await;
    let revoked = api.send(Method::DELETE, &format!("/api/v1/tokens/{id}"), token, &[], None).await;
    let after = api.send(Method::GET, "/api/v1/me", Some(&secret), &[], None).await;
    let again = api.send(Method::DELETE, &format!("/api/v1/tokens/{id}"), token, &[], None).await;

    assert_eq!(created.status, StatusCode::CREATED);
    assert!(secret.starts_with("spk_"));
    assert_eq!(created.body["api_token"]["role"], "developer");
    assert_eq!(used.status, StatusCode::OK);
    let names: Vec<_> = listed.body["items"].as_array().unwrap().iter().map(|token| token["name"].clone()).collect();
    assert!(names.contains(&json!("CI")), "{names:?}");
    assert_eq!(wider.status, StatusCode::FORBIDDEN);
    assert_eq!(revoked.status, StatusCode::NO_CONTENT);
    assert_eq!(after.status, StatusCode::UNAUTHORIZED);
    assert_eq!(again.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn sets_a_source_and_deploys_it() {
    let api = Api::new().await;
    let bearer = api.admin_token().await;
    let token = Some(bearer.as_str());
    let project = api.send(Method::POST, "/api/v1/projects", token, &[], Some(json!({ "name": "Shop" }))).await;
    let environments = format!("/api/v1/projects/{}/environments", project.body["id"].as_str().unwrap());
    let production =
        api.send(Method::POST, &environments, token, &[], Some(json!({ "name": "Production", "kind": "production" }))).await;
    let server = api.send(Method::POST, "/api/v1/servers", token, &[], Some(json!({ "name": "web-1" }))).await;
    let services = format!("/api/v1/environments/{}/services", production.body["id"].as_str().unwrap());
    let web = json!({ "name": "Web", "kind": "app", "config": { "runtime": { "port": 3000 } }, "server_id": server.body["id"] });
    let service = api.send(Method::POST, &services, token, &[], Some(web)).await;
    let source = format!("/api/v1/services/{}/source", service.body["id"].as_str().unwrap());
    let deployments = format!("/api/v1/services/{}/deployments", service.body["id"].as_str().unwrap());

    let no_source = api.send(Method::POST, &deployments, token, &[], None).await;
    let typo = api.send(Method::POST, &source, token, &[], Some(json!({ "type": "image", "imag": "web:1" }))).await;
    let created = api.send(Method::POST, &source, token, &[], Some(json!({ "type": "image", "image": "web:1" }))).await;
    let git = json!({ "type": "git", "repo_url": "https://github.com/acme/web.git", "branch": "main" });
    let replaced = api.send(Method::PUT, &source, token, &[(header::IF_MATCH, created.header(header::ETAG))], Some(git)).await;
    let deployed = api.send(Method::POST, &deployments, token, &[], None).await;
    let location = deployed.header(header::LOCATION).to_string();
    let listed = api.send(Method::GET, &deployments, token, &[], None).await;
    let canceled = api.send(Method::POST, &format!("{location}/cancel"), token, &[], None).await;
    let shown = api.send(Method::GET, &location, token, &[], None).await;
    let events = api.send(Method::GET, &format!("{location}/events"), token, &[], None).await;
    let again = api.send(Method::POST, &format!("{location}/cancel"), token, &[], None).await;
    let (stream_status, stream_type, streamed) = api.get_text(&format!("{location}/stream"), &bearer).await;
    let service_logs = format!("/api/v1/services/{}/logs", service.body["id"].as_str().unwrap());
    let nothing_live = api.send(Method::GET, &service_logs, token, &[], None).await;
    let deployment_id = deployed.body["id"].as_str().unwrap();
    let no_container =
        api.send(Method::GET, &format!("{service_logs}?deployment_id={deployment_id}&tail=5"), token, &[], None).await;
    let unknown_param = api.send(Method::GET, &format!("{service_logs}?lines=5"), token, &[], None).await;

    assert_eq!((no_source.status, &no_source.body["field"]), (StatusCode::UNPROCESSABLE_ENTITY, &json!("source")));
    assert_eq!((typo.status, &typo.body["code"]), (StatusCode::UNPROCESSABLE_ENTITY, &json!("invalid_body")));
    assert_eq!((created.status, &created.body["image"]), (StatusCode::CREATED, &json!("web:1")));
    assert_eq!((replaced.status, &replaced.body["type"]), (StatusCode::OK, &json!("git")));
    assert_eq!((deployed.status, &deployed.body["status"]), (StatusCode::CREATED, &json!("queued")));
    assert_eq!(deployed.body["server_id"], server.body["id"]);
    assert_eq!(deployed.body["snapshot"]["source"]["branch"], "main");
    assert_eq!(listed.body["items"][0]["id"], deployed.body["id"]);
    assert_eq!((canceled.status, &canceled.body["reason"]), (StatusCode::OK, &json!("canceled_by_user")));
    assert_eq!(shown.body["status"], "canceled");
    let path: Vec<_> = events.body.as_array().unwrap().iter().map(|event| event["to"].as_str().unwrap()).collect();
    assert_eq!(path, ["queued", "canceled"]);
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!((stream_status, stream_type.as_str()), (StatusCode::OK, "text/event-stream"));
    assert!(
        streamed.starts_with("event: status\ndata: {\"message\":null,\"reason\":\"canceled_by_user\",\"status\":\"canceled\"}"),
        "{streamed}"
    );
    assert!(streamed.ends_with("event: end\ndata: canceled\n\n"), "{streamed:?}");
    assert_eq!((nothing_live.status, &nothing_live.body["field"]), (StatusCode::UNPROCESSABLE_ENTITY, &json!("deployment_id")));
    assert_eq!(no_container.status, StatusCode::CONFLICT, "{:?}", no_container.body);
    assert_eq!(unknown_param.status, StatusCode::BAD_REQUEST, "{:?}", unknown_param.body);

    let deploy_key = format!("{source}/deploy-key");
    let logs = api.send(Method::GET, &format!("{location}/logs"), token, &[], None).await;
    let no_key = api.send(Method::GET, &deploy_key, token, &[], None).await;
    let ssh = json!({ "type": "git", "repo_url": "git@github.com:acme/web.git", "branch": "main" });
    api.send(Method::PUT, &source, token, &[(header::IF_MATCH, replaced.header(header::ETAG))], Some(ssh)).await;
    let key = api.send(Method::GET, &deploy_key, token, &[], None).await;
    let rotated = api.send(Method::POST, &deploy_key, token, &[], None).await;

    assert_eq!((logs.status, logs.header(header::CONTENT_TYPE)), (StatusCode::OK, "text/plain; charset=utf-8"));
    assert_eq!(no_key.status, StatusCode::NOT_FOUND);
    assert!(key.body["public_key"].as_str().unwrap().starts_with("ssh-ed25519 "), "{:?}", key.body);
    assert!(key.body["fingerprint"].as_str().unwrap().starts_with("SHA256:"));
    assert_eq!((rotated.status, rotated.header(header::LOCATION)), (StatusCode::CREATED, deploy_key.as_str()));
    assert_ne!(rotated.body["fingerprint"], key.body["fingerprint"]);

    let rollback = format!("/api/v1/services/{}/rollback", service.body["id"].as_str().unwrap());
    let nothing_earlier = api.send(Method::POST, &rollback, token, &[], None).await;
    let to_canceled = api.send(Method::POST, &rollback, token, &[], Some(json!({ "deployment_id": deployed.body["id"] }))).await;
    let typo = api.send(Method::POST, &rollback, token, &[], Some(json!({ "deployment": deployed.body["id"] }))).await;
    let unknown = format!("dep_{}", "0".repeat(26));
    let unknown = api.send(Method::POST, &rollback, token, &[], Some(json!({ "deployment_id": unknown }))).await;

    assert_eq!(
        (nothing_earlier.status, &nothing_earlier.body["field"]),
        (StatusCode::UNPROCESSABLE_ENTITY, &json!("deployment_id"))
    );
    assert!(to_canceled.body["detail"].as_str().unwrap().contains("is canceled"), "{:?}", to_canceled.body);
    assert_eq!((typo.status, &typo.body["code"]), (StatusCode::UNPROCESSABLE_ENTITY, &json!("invalid_body")));
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);

    // A restart needs something live; the control plane's tests cover the rest.
    let restart = format!("/api/v1/services/{}/restart", service.body["id"].as_str().unwrap());
    let nothing_live = api.send(Method::POST, &restart, token, &[], None).await;
    assert_eq!(nothing_live.status, StatusCode::CONFLICT);
    assert!(nothing_live.body["detail"].as_str().unwrap().contains("nothing live to restart"), "{:?}", nothing_live.body);
}

#[tokio::test]
async fn manages_domains_and_the_base_domain() {
    let api = Api::new().await;
    let bearer = api.admin_token().await;
    let token = Some(bearer.as_str());
    let project = api.send(Method::POST, "/api/v1/projects", token, &[], Some(json!({ "name": "Shop" }))).await;
    let environments = format!("/api/v1/projects/{}/environments", project.body["id"].as_str().unwrap());
    let production =
        api.send(Method::POST, &environments, token, &[], Some(json!({ "name": "Production", "kind": "production" }))).await;
    let services = format!("/api/v1/environments/{}/services", production.body["id"].as_str().unwrap());
    let service = api.send(Method::POST, &services, token, &[], Some(json!({ "name": "Web", "kind": "app" }))).await;
    let domains = format!("/api/v1/services/{}/domains", service.body["id"].as_str().unwrap());

    let base = api.send(Method::GET, "/api/v1/settings/base_domain", token, &[], None).await;
    let set = api
        .send(
            Method::PUT,
            "/api/v1/settings/base_domain",
            token,
            &[(header::IF_MATCH, base.header(header::ETAG))],
            Some(json!({ "value": "apps.example.com" })),
        )
        .await;
    let unknown = api.send(Method::GET, "/api/v1/settings/theme", token, &[], None).await;
    let apex = api.send(Method::POST, &domains, token, &[], Some(json!({ "hostname": "shop.example.com" }))).await;
    let www = json!({ "hostname": "www.shop.example.com", "redirect_to": apex.body["id"] });
    let www = api.send(Method::POST, &domains, token, &[], Some(www)).await;
    let reserved = api.send(Method::POST, &domains, token, &[], Some(json!({ "hostname": "x.apps.example.com" }))).await;
    let www_path = www.header(header::LOCATION).to_string();
    let unredirected = api
        .send(
            Method::PATCH,
            &www_path,
            token,
            &[(header::IF_MATCH, www.header(header::ETAG))],
            Some(json!({ "redirect_to": null })),
        )
        .await;
    let listed = api.send(Method::GET, &domains, token, &[], None).await;
    let deleted = api.send(Method::DELETE, &www_path, token, &[], None).await;

    assert_eq!((base.body["value"].as_str(), base.header(header::ETAG)), (Some("localhost"), "\"0\""));
    assert_eq!((set.status, &set.body["value"], set.header(header::ETAG)), (StatusCode::OK, &json!("apps.example.com"), "\"1\""));
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(apex.status, StatusCode::CREATED);
    assert_eq!(www.body["redirect_to"], apex.body["id"]);
    assert_eq!((reserved.status, &reserved.body["field"]), (StatusCode::UNPROCESSABLE_ENTITY, &json!("hostname")));
    assert_eq!((unredirected.status, &unredirected.body["redirect_to"]), (StatusCode::OK, &json!(null)));
    assert_eq!(listed.body["generated_hostname"], "web-production-shop.apps.example.com");
    assert_eq!(listed.body["domains"].as_array().unwrap().len(), 2);
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn a_webhook_deploys_pushes() {
    let api = Api::new().await;
    let bearer = api.admin_token().await;
    let token = Some(bearer.as_str());
    let project = api.send(Method::POST, "/api/v1/projects", token, &[], Some(json!({ "name": "Shop" }))).await;
    let environments = format!("/api/v1/projects/{}/environments", project.body["id"].as_str().unwrap());
    let production =
        api.send(Method::POST, &environments, token, &[], Some(json!({ "name": "Production", "kind": "production" }))).await;
    let server = api.send(Method::POST, "/api/v1/servers", token, &[], Some(json!({ "name": "web-1" }))).await;
    let services = format!("/api/v1/environments/{}/services", production.body["id"].as_str().unwrap());
    let web = json!({ "name": "Web", "kind": "app", "config": {}, "server_id": server.body["id"] });
    let service = api.send(Method::POST, &services, token, &[], Some(web)).await;
    let source = format!("/api/v1/services/{}/source", service.body["id"].as_str().unwrap());
    let git = json!({ "type": "git", "repo_url": "https://gitlab.com/acme/web.git", "branch": "main" });
    api.send(Method::POST, &source, token, &[], Some(git)).await;
    let webhook = format!("{source}/webhook");
    let commit = "abcdef0123456789abcdef0123456789abcdef01";
    let push = json!({
        "object_kind": "push", "ref": "refs/heads/main", "before": "1111111111111111111111111111111111111111",
        "after": commit, "total_commits_count": 1, "commits": [{ "added": ["a"], "modified": [], "removed": [] }],
    });
    let (event, signature) =
        (header::HeaderName::from_static("x-gitlab-event"), header::HeaderName::from_static("x-gitlab-token"));
    // A GitLab delivery of the push to `path`, with `secret` as its token.
    let deliver = |path: String, secret: String| {
        let (push, event, signature) = (push.clone(), event.clone(), signature.clone());
        let api = &api;
        async move { api.send(Method::POST, &path, None, &[(event, "Push Hook"), (signature, &secret)], Some(push)).await }
    };

    let none = api.send(Method::GET, &webhook, token, &[], None).await;
    let created = api.send(Method::POST, &webhook, token, &[], None).await;
    let secret = created.body["secret"].as_str().unwrap().to_string();
    let hook = created.body["paths"]["gitlab"].as_str().unwrap().to_string();
    let forged = deliver(hook.clone(), "guess".to_string()).await;
    let deployed = deliver(hook.clone(), secret.clone()).await;
    let again = deliver(hook.clone(), secret.clone()).await;
    let other_provider = deliver(hook.replace("/gitlab/", "/bitbucket/"), secret.clone()).await;
    let shown = api.send(Method::GET, &webhook, token, &[], None).await;
    let deleted = api.send(Method::DELETE, &webhook, token, &[], None).await;
    let after_delete = deliver(hook.clone(), secret.clone()).await;

    assert_eq!(none.status, StatusCode::NOT_FOUND);
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(forged.status, StatusCode::UNAUTHORIZED);
    assert_eq!((deployed.status, &deployed.body["result"]), (StatusCode::ACCEPTED, &json!("deployed")));
    let deployment = format!("/api/v1/deployments/{}", deployed.body["deployment_id"].as_str().unwrap());
    let deployment = api.send(Method::GET, &deployment, token, &[], None).await;
    assert_eq!((&deployment.body["trigger"], &deployment.body["commit_sha"]), (&json!("git_push"), &json!(commit)));
    assert_eq!((again.status, &again.body["result"]), (StatusCode::OK, &json!("ignored")));
    assert_eq!(other_provider.status, StatusCode::NOT_FOUND);
    assert!(shown.body.get("secret").is_none(), "{:?}", shown.body);
    assert!(shown.body["last_delivery"].as_str().unwrap().starts_with("ignored: "), "{:?}", shown.body);
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_eq!(after_delete.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn manages_variables_with_write_only_secrets_and_volumes() {
    let api = Api::new().await;
    let bearer = api.admin_token().await;
    let token = Some(bearer.as_str());
    let project = api.send(Method::POST, "/api/v1/projects", token, &[], Some(json!({ "name": "Shop" }))).await;
    let project_id = project.body["id"].as_str().unwrap();
    let environments = format!("/api/v1/projects/{project_id}/environments");
    let production =
        api.send(Method::POST, &environments, token, &[], Some(json!({ "name": "Production", "kind": "production" }))).await;
    let services = format!("/api/v1/environments/{}/services", production.body["id"].as_str().unwrap());
    let service =
        api.send(Method::POST, &services, token, &[], Some(json!({ "name": "Web", "kind": "app", "config": {} }))).await;
    let service_id = service.body["id"].as_str().unwrap();
    let variables = format!("/api/v1/services/{service_id}/variables");

    let secret = json!({ "name": "API_KEY", "value": "s3cret", "secret": true });
    let created = api.send(Method::POST, &variables, token, &[], Some(secret)).await;
    let plain = json!({ "name": "LOG", "value": "info", "availability": "both" });
    let on_project = api.send(Method::POST, &format!("/api/v1/projects/{project_id}/variables"), token, &[], Some(plain)).await;
    let typo = api.send(Method::POST, &variables, token, &[], Some(json!({ "name": "X", "valeu": "1" }))).await;
    let location = created.header(header::LOCATION).to_string();
    let listed = api.send(Method::GET, &variables, token, &[], None).await;
    let etag = [(header::IF_MATCH, created.header(header::ETAG))];
    let renamed = api.send(Method::PATCH, &location, token, &etag, Some(json!({ "name": "API_TOKEN" }))).await;
    let revealed = api.send(Method::POST, &format!("{location}/reveal"), token, &[], None).await;
    let deleted = api.send(Method::DELETE, &location, token, &[], None).await;
    let gone = api.send(Method::GET, &location, token, &[], None).await;

    assert_eq!(
        (created.status, &created.body["value"], &created.body["scope"]),
        (StatusCode::CREATED, &Value::Null, &json!("service"))
    );
    assert_eq!((on_project.status, &on_project.body["value"]), (StatusCode::CREATED, &json!("info")));
    assert_eq!((typo.status, &typo.body["code"]), (StatusCode::UNPROCESSABLE_ENTITY, &json!("invalid_body")));
    assert_eq!(listed.body.as_array().unwrap().len(), 1);
    assert_eq!(
        (renamed.status, &renamed.body["name"], &renamed.body["value"]),
        (StatusCode::OK, &json!("API_TOKEN"), &Value::Null)
    );
    assert_eq!((revealed.body["value"].as_str(), revealed.header(header::CACHE_CONTROL)), (Some("s3cret"), "no-store"));
    assert_eq!((deleted.status, gone.status), (StatusCode::NO_CONTENT, StatusCode::NOT_FOUND));

    let volumes = format!("/api/v1/services/{service_id}/volumes");
    let volume = api.send(Method::POST, &volumes, token, &[], Some(json!({ "name": "data", "mount_path": "/data/" }))).await;
    let bad = api.send(Method::POST, &volumes, token, &[], Some(json!({ "name": "x", "mount_path": "data" }))).await;
    let volume_location = volume.header(header::LOCATION).to_string();
    let etag = [(header::IF_MATCH, volume.header(header::ETAG))];
    let moved = api.send(Method::PATCH, &volume_location, token, &etag, Some(json!({ "mount_path": "/srv" }))).await;
    let listed = api.send(Method::GET, &volumes, token, &[], None).await;
    let removed = api.send(Method::DELETE, &volume_location, token, &[], None).await;

    assert_eq!((volume.status, &volume.body["mount_path"]), (StatusCode::CREATED, &json!("/data")));
    assert_eq!((bad.status, &bad.body["field"]), (StatusCode::UNPROCESSABLE_ENTITY, &json!("mount_path")));
    assert_eq!(moved.body["mount_path"], "/srv");
    assert_eq!(listed.body[0]["mount_path"], "/srv");
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn creates_a_database_with_its_parts() {
    let api = Api::new().await;
    let bearer = api.admin_token().await;
    let token = Some(bearer.as_str());
    let project = api.send(Method::POST, "/api/v1/projects", token, &[], Some(json!({ "name": "Shop" }))).await;
    let environments = format!("/api/v1/projects/{}/environments", project.body["id"].as_str().unwrap());
    let production =
        api.send(Method::POST, &environments, token, &[], Some(json!({ "name": "Production", "kind": "production" }))).await;
    let server = api.send(Method::POST, "/api/v1/servers", token, &[], Some(json!({ "name": "db-1" }))).await;
    let databases = format!("/api/v1/environments/{}/databases", production.body["id"].as_str().unwrap());

    let redis = json!({ "name": "Cache", "engine": "redis", "server_id": server.body["id"] });
    let created = api.send(Method::POST, &databases, token, &[], Some(redis)).await;
    let unknown_engine = api.send(Method::POST, &databases, token, &[], Some(json!({ "name": "X", "engine": "oracle" }))).await;
    let bad_version =
        api.send(Method::POST, &databases, token, &[], Some(json!({ "name": "X", "engine": "postgres", "version": "9" }))).await;
    let no_engine = api.send(Method::POST, &databases, token, &[], Some(json!({ "name": "X" }))).await;
    let unknown_backup = json!({ "name": "X", "from_backup": "bkp_01J0000000000000000000000Z" });
    let unknown_backup = api.send(Method::POST, &databases, token, &[], Some(unknown_backup)).await;
    let location = created.header(header::LOCATION).to_string();
    let variables = api.send(Method::GET, &format!("{location}/variables"), token, &[], None).await;
    let volumes = api.send(Method::GET, &format!("{location}/volumes"), token, &[], None).await;
    let data = format!("/api/v1/volumes/{}", volumes.body[0]["id"].as_str().unwrap());
    let revision = format!("\"{}\"", volumes.body[0]["revision"]);
    let moved =
        api.send(Method::PATCH, &data, token, &[(header::IF_MATCH, &revision)], Some(json!({ "mount_path": "/x" }))).await;

    assert_eq!((moved.status, &moved.body["code"]), (StatusCode::CONFLICT, &json!("rejected")), "{:?}", moved.body);
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!((&created.body["service"]["kind"], &created.body["service"]["slug"]), (&json!("database"), &json!("cache")));
    assert_eq!(created.body["service"]["config"]["database"], json!({ "engine": "redis", "version": "8" }));
    assert_eq!(
        (&created.body["deployment"]["status"], &created.body["deployment"]["strategy"]),
        (&json!("queued"), &json!("recreate"))
    );
    let names: Vec<_> = variables.body.as_array().unwrap().iter().map(|variable| variable["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["REDIS_PASSWORD", "REDIS_URL"]);
    assert!(variables.body.as_array().unwrap().iter().all(|variable| variable["value"].is_null()));
    assert_eq!(volumes.body[0]["mount_path"], "/data");
    assert_eq!(unknown_engine.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!((bad_version.status, &bad_version.body["field"]), (StatusCode::UNPROCESSABLE_ENTITY, &json!("version")));
    assert_eq!((no_engine.status, &no_engine.body["field"]), (StatusCode::UNPROCESSABLE_ENTITY, &json!("engine")));
    assert_eq!(unknown_backup.status, StatusCode::NOT_FOUND, "{:?}", unknown_backup.body);
    assert!(created.body.get("restore").is_none(), "only when it starts from a backup");
}

#[tokio::test]
async fn manages_destinations_schedules_and_backups_of_a_database() {
    let api = Api::new().await;
    let bearer = api.admin_token().await;
    let token = Some(bearer.as_str());
    let project = api.send(Method::POST, "/api/v1/projects", token, &[], Some(json!({ "name": "Shop" }))).await;
    let environments = format!("/api/v1/projects/{}/environments", project.body["id"].as_str().unwrap());
    let production =
        api.send(Method::POST, &environments, token, &[], Some(json!({ "name": "Production", "kind": "production" }))).await;
    let server = api.send(Method::POST, "/api/v1/servers", token, &[], Some(json!({ "name": "db-1" }))).await;
    let databases = format!("/api/v1/environments/{}/databases", production.body["id"].as_str().unwrap());
    let postgres = json!({ "name": "Postgres", "engine": "postgres", "server_id": server.body["id"] });
    let database = api.send(Method::POST, &databases, token, &[], Some(postgres)).await;
    let service = database.header(header::LOCATION).to_string();

    let new = json!({
        "name": "R2", "endpoint": "https://account.r2.cloudflarestorage.com", "region": "auto", "bucket": "backups",
        "path_prefix": "spinikus", "access_key_id": "key", "secret_access_key": "s3cret",
    });
    let destination = api.send(Method::POST, "/api/v1/storage-destinations", token, &[], Some(new)).await;
    let location = destination.header(header::LOCATION).to_string();
    let typo = api
        .send(Method::POST, "/api/v1/storage-destinations", token, &[], Some(json!({ "name": "X", "bucket": "b", "secrets": 1 })))
        .await;
    let tested = api.send(Method::POST, &format!("{location}/test"), token, &[], None).await;
    let rotate = Some(json!({ "secret_access_key": "n3w", "endpoint": null }));
    let rotated = api.send(Method::PATCH, &location, token, &[(header::IF_MATCH, "\"1\"")], rotate).await;
    let id = destination.body["id"].clone();
    let schedules = format!("{service}/backup-schedules");
    let schedule =
        api.send(Method::POST, &schedules, token, &[], Some(json!({ "destination_id": id, "cron": "30 3 * * *" }))).await;
    let bad_cron = api.send(Method::POST, &schedules, token, &[], Some(json!({ "destination_id": id, "cron": "daily" }))).await;
    let in_use = api.send(Method::DELETE, &location, token, &[], None).await;
    let backups = format!("{service}/backups");
    let backup = api.send(Method::POST, &backups, token, &[], Some(json!({ "destination_id": id }))).await;
    let listed = api.send(Method::GET, &backups, token, &[], None).await;
    let running = api.send(Method::DELETE, backup.header(header::LOCATION), token, &[], None).await;

    assert_eq!(destination.status, StatusCode::CREATED);
    assert_eq!(destination.body["path_prefix"], "spinikus/");
    assert!(destination.body.get("secret_access_key").is_none(), "{}", destination.body);
    assert_eq!(typo.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(tested.status, StatusCode::NO_CONTENT);
    assert_eq!((rotated.status, &rotated.body["endpoint"]), (StatusCode::OK, &Value::Null));
    assert_eq!(rotated.header(header::ETAG), "\"2\"");
    assert_eq!(schedule.status, StatusCode::CREATED);
    assert_eq!((&schedule.body["keep_last"], &schedule.body["enabled"]), (&json!(7), &json!(true)));
    assert!(schedule.body["next_run_at"].as_str().unwrap().ends_with("03:30:00Z"), "{}", schedule.body);
    assert_eq!((bad_cron.status, &bad_cron.body["field"]), (StatusCode::UNPROCESSABLE_ENTITY, &json!("cron")));
    assert_eq!(in_use.status, StatusCode::CONFLICT);
    assert_eq!(backup.status, StatusCode::CREATED);
    assert_eq!((&backup.body["status"], &backup.body["engine"]), (&json!("queued"), &json!("postgres")));
    assert!(backup.body["object_key"].as_str().unwrap().ends_with(".pgdump"));
    assert_eq!(listed.body["items"][0]["id"], backup.body["id"]);
    assert_eq!((running.status, &running.body["code"]), (StatusCode::CONFLICT, &json!("rejected")));

    // Only a backup that succeeded can be restored, and the database must be live.
    let restore = api.send(Method::POST, &format!("{}/restore", backup.header(header::LOCATION)), token, &[], None).await;
    let restores = api.send(Method::GET, &format!("{service}/restores"), token, &[], None).await;
    let unknown = api.send(Method::GET, "/api/v1/restores/rst_01JZ0000000000000000000000", token, &[], None).await;
    assert_eq!((restore.status, &restore.body["code"]), (StatusCode::CONFLICT, &json!("rejected")));
    assert_eq!(restores.body["items"], json!([]));
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn admins_manage_users_members_and_read_the_audit_log() {
    let api = Api::new().await;
    let bearer = api.admin_token().await;
    let token = Some(bearer.as_str());
    let login = |email: &str, password: &str| {
        api.send(Method::POST, "/api/v1/login", None, &[], Some(json!({ "email": email, "password": password })))
    };

    let new_user = json!({ "email": "grace@example.com", "name": "Grace", "password": "an initial password" });
    let created = api.send(Method::POST, "/api/v1/users", token, &[], Some(new_user)).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let id = created.body["id"].as_str().unwrap().to_string();
    let etag = created.header(header::ETAG).to_string();
    let renamed = api
        .send(
            Method::PATCH,
            &format!("/api/v1/users/{id}"),
            token,
            &[(header::IF_MATCH, &etag)],
            Some(json!({ "name": "Grace H." })),
        )
        .await;
    assert_eq!((renamed.status, &renamed.body["name"]), (StatusCode::OK, &json!("Grace H.")));
    let grace = login("grace@example.com", "an initial password").await.body["token"].as_str().unwrap().to_string();
    let changed = api
        .send(
            Method::PUT,
            "/api/v1/me/password",
            Some(&grace),
            &[],
            Some(json!({ "current_password": "an initial password", "new_password": "grace's own password" })),
        )
        .await;
    assert_eq!(changed.status, StatusCode::NO_CONTENT);
    assert_eq!(login("grace@example.com", "grace's own password").await.status, StatusCode::CREATED);
    assert_eq!(api.send(Method::GET, "/api/v1/users", Some(&grace), &[], None).await.status, StatusCode::FORBIDDEN);
    let tokens = api.send(Method::GET, &format!("/api/v1/users/{id}/tokens"), token, &[], None).await;
    assert_eq!(tokens.body["items"].as_array().unwrap().len(), 2, "two logins");

    // Members of a project, by email.
    let project = api.send(Method::POST, "/api/v1/projects", token, &[], Some(json!({ "name": "Shop" }))).await;
    let members = format!("/api/v1/projects/{}/members", project.body["id"].as_str().unwrap());
    let added =
        api.send(Method::POST, &members, token, &[], Some(json!({ "email": "grace@example.com", "role": "viewer" }))).await;
    assert_eq!(added.status, StatusCode::CREATED, "{}", added.body);
    assert_eq!(
        (&added.body["user"]["name"], &added.body["role"], &added.body["scope"]),
        (&json!("Grace H."), &json!("viewer"), &json!("project"))
    );
    let membership = added.body["id"].as_str().unwrap().to_string();
    let etag = added.header(header::ETAG).to_string();
    let promoted = api
        .send(
            Method::PATCH,
            &format!("/api/v1/memberships/{membership}"),
            token,
            &[(header::IF_MATCH, &etag)],
            Some(json!({ "role": "admin" })),
        )
        .await;
    assert_eq!(promoted.body["role"], "admin");
    let listed = api.send(Method::GET, &members, Some(&grace), &[], None).await;
    assert_eq!(listed.body["items"].as_array().unwrap().len(), 1, "a project admin lists its members");
    let unknown =
        api.send(Method::POST, &members, token, &[], Some(json!({ "email": "nobody@example.com", "role": "viewer" }))).await;
    assert_eq!((unknown.status, &unknown.body["code"]), (StatusCode::UNPROCESSABLE_ENTITY, &json!("invalid")));

    // The project's admins read its part of the log; only instance admins read all of it.
    let project_log = format!("/api/v1/audit-log?project_id={}", project.body["id"].as_str().unwrap());
    let log = api.send(Method::GET, &project_log, Some(&grace), &[], None).await;
    let actions: Vec<_> = log.body["items"].as_array().unwrap().iter().map(|entry| entry["action"].clone()).collect();
    assert_eq!(actions, [json!("membership.update"), json!("membership.create"), json!("project.create")]);
    assert_eq!(log.body["users"][0]["email"], "admin@example.com", "the actors come along");
    assert_eq!(api.send(Method::GET, "/api/v1/audit-log", Some(&grace), &[], None).await.status, StatusCode::FORBIDDEN);
    let all = api.send(Method::GET, "/api/v1/audit-log?limit=2", token, &[], None).await;
    assert_eq!(all.body["items"].as_array().unwrap().len(), 2);
    let older = api
        .send(
            Method::GET,
            &format!("/api/v1/audit-log?limit=2&before={}", all.body["next_cursor"].as_str().unwrap()),
            token,
            &[],
            None,
        )
        .await;
    assert_ne!(older.body["items"][0]["id"], all.body["items"][0]["id"]);

    let removed = api.send(Method::DELETE, &format!("/api/v1/memberships/{membership}"), token, &[], None).await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
    let disabled = api.send(Method::POST, &format!("/api/v1/users/{id}/disable"), token, &[], None).await;
    assert!(disabled.body["disabled_at"].is_string());
    assert_eq!(api.send(Method::GET, "/api/v1/me", Some(&grace), &[], None).await.status, StatusCode::UNAUTHORIZED);
    let reset = api
        .send(Method::PUT, &format!("/api/v1/users/{id}/password"), token, &[], Some(json!({ "password": "a reset password" })))
        .await;
    assert_eq!(reset.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn instance_admins_schedule_backups_of_the_instance() {
    let api = Api::new().await;
    let bearer = api.admin_token().await;
    let token = Some(bearer.as_str());
    let schedule = "/api/v1/instance-backup-schedule";

    let none = api.send(Method::GET, schedule, token, &[], None).await;
    let early = api.send(Method::POST, "/api/v1/instance-backups", token, &[], None).await;
    let new = json!({ "name": "Offsite", "bucket": "backups", "access_key_id": "key", "secret_access_key": "s3cret" });
    let destination = api.send(Method::POST, "/api/v1/storage-destinations", token, &[], Some(new)).await;
    let id = destination.body["id"].clone();
    let body = |passphrase: Option<&str>| {
        let mut body = json!({ "destination_id": id, "cron": "0 3 * * *", "keep_last": 14 });
        if let Some(passphrase) = passphrase {
            body["passphrase"] = json!(passphrase);
        }
        Some(body)
    };
    let unconditional = api.send(Method::PUT, schedule, token, &[], body(Some("a long enough passphrase"))).await;
    let no_passphrase = api.send(Method::PUT, schedule, token, &[(header::IF_MATCH, "\"0\"")], body(None)).await;
    let set =
        api.send(Method::PUT, schedule, token, &[(header::IF_MATCH, "\"0\"")], body(Some("a long enough passphrase"))).await;
    let kept = api.send(Method::PUT, schedule, token, &[(header::IF_MATCH, "\"1\"")], body(None)).await;
    let shown = api.send(Method::GET, schedule, token, &[], None).await;
    let backup = api.send(Method::POST, "/api/v1/instance-backups", token, &[], None).await;
    let listed = api.send(Method::GET, "/api/v1/instance-backups", token, &[], None).await;
    let running = api.send(Method::DELETE, backup.header(header::LOCATION), token, &[], None).await;

    assert_eq!(none.status, StatusCode::NOT_FOUND);
    assert_eq!((early.status, &early.body["code"]), (StatusCode::CONFLICT, &json!("rejected")));
    assert_eq!(unconditional.status, StatusCode::PRECONDITION_REQUIRED);
    assert_eq!((no_passphrase.status, &no_passphrase.body["field"]), (StatusCode::UNPROCESSABLE_ENTITY, &json!("passphrase")));
    assert_eq!((set.status, set.header(header::ETAG)), (StatusCode::OK, "\"1\""), "{}", set.body);
    assert_eq!((&set.body["keep_last"], &set.body["enabled"]), (&json!(14), &json!(true)));
    assert!(set.body["next_run_at"].as_str().unwrap().ends_with("03:00:00Z"), "{}", set.body);
    assert!(set.body.get("passphrase").is_none(), "write-only: {}", set.body);
    assert_eq!(kept.header(header::ETAG), "\"2\"");
    assert_eq!(shown.body["destination_id"], id);
    assert_eq!(
        (backup.status, &backup.body["status"], &backup.body["scheduled"]),
        (StatusCode::CREATED, &json!("queued"), &json!(false))
    );
    assert!(backup.body["object_key"].as_str().unwrap().starts_with("instance/ibk_"), "{}", backup.body);
    assert_eq!(listed.body["items"][0]["id"], backup.body["id"]);
    assert_eq!((running.status, &running.body["code"]), (StatusCode::CONFLICT, &json!("rejected")));
    // The destination is in use.
    let in_use = api.send(Method::DELETE, destination.header(header::LOCATION), token, &[], None).await;
    assert_eq!(in_use.status, StatusCode::CONFLICT);

    // Not for anyone else.
    api.send(
        Method::POST,
        "/api/v1/users",
        token,
        &[],
        Some(json!({ "email": "grace@example.com", "name": "Grace", "password": "an initial password" })),
    )
    .await;
    let login = api
        .send(
            Method::POST,
            "/api/v1/login",
            None,
            &[],
            Some(json!({ "email": "grace@example.com", "password": "an initial password" })),
        )
        .await;
    let grace = login.body["token"].as_str().unwrap().to_string();
    let denied = api.send(Method::GET, "/api/v1/instance-backups", Some(&grace), &[], None).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
}
