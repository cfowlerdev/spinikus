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
        let control = Control::new(SqliteStore::open(pool, &MasterKey::generate()).await.unwrap());
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

    async fn send(
        &self,
        method: Method,
        uri: &str,
        token: Option<&str>,
        headers: &[(header::HeaderName, &str)],
        body: Option<Value>,
    ) -> Reply {
        let mut request = Request::builder().method(method).uri(uri);
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

    assert_eq!(before.body, json!({ "needs_setup": true }));
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(created.body["email"], "ada@example.com");
    assert!(created.body.get("password").is_none());
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.header(header::CONTENT_TYPE), "application/problem+json");
    assert_eq!(again.body["code"], "already_set_up");
    assert_eq!(again.body["status"], 409);
}

#[tokio::test]
async fn logs_in_for_a_bearer_token() {
    let api = Api::new().await;
    let token = api.admin_token().await;
    let wrong = json!({ "email": "admin@example.com", "password": "not the password" });

    let failed = api.send(Method::POST, "/api/v1/login", None, &[], Some(wrong)).await;
    let me = api.send(Method::GET, "/api/v1/me", Some(&token), &[], None).await;
    let anonymous = api.send(Method::GET, "/api/v1/me", None, &[], None).await;
    let bogus = api.send(Method::GET, "/api/v1/me", Some("spk_bogus"), &[], None).await;

    assert!(token.starts_with("spk_"));
    assert_eq!((failed.status, &failed.body["code"]), (StatusCode::UNAUTHORIZED, &json!("invalid_credentials")));
    assert_eq!(me.body["email"], "admin@example.com");
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    assert_eq!(anonymous.header(header::WWW_AUTHENTICATE), "Bearer");
    assert_eq!(bogus.body["code"], "unauthenticated");
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
    let services = format!("/api/v1/environments/{}/services", production.body["id"].as_str().unwrap());
    let typo = api.send(Method::POST, &services, token, &[], Some(json!({ "name": "Web", "kind": "app", "conifg": {} }))).await;
    let web = json!({ "name": "Web", "kind": "app", "config": { "port": 8080 } });
    let created = api.send(Method::POST, &services, token, &[], Some(web)).await;
    let listed = api.send(Method::GET, &services, token, &[], None).await;

    assert_eq!((no_pr.status, &no_pr.body["field"]), (StatusCode::UNPROCESSABLE_ENTITY, &json!("kind")));
    assert_eq!(production.body["kind"], "production");
    assert_eq!((typo.status, &typo.body["code"]), (StatusCode::UNPROCESSABLE_ENTITY, &json!("invalid_body")));
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(created.body["config"], json!({ "port": 8080 }));
    assert_eq!(listed.body["items"].as_array().unwrap().len(), 1);
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
