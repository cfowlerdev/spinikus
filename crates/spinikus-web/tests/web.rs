//! The dashboard over HTTP requests, against a real service layer and database.

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Method, Request, StatusCode, header},
};
use http_body_util::BodyExt;
use spinikus_control::{Control, NewEnvironment, NewProject, NewServer, NewService, password};
use spinikus_core::{
    actor::Actor,
    deployment::DeploymentStatus,
    environment::EnvironmentKind,
    membership::{MembershipScope, Role},
    secret::Secret,
    service::{ServiceConfig, ServiceKind},
    source::{GitSource, ImageSource, SourceSpec},
    user::UserFields,
};
use spinikus_store::{DeploymentRepository, MasterKey, MembershipRepository, SqliteStore, UserRepository, connect, migrate};
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery staple";

struct Web {
    _dir: tempfile::TempDir,
    control: Control,
    router: Router,
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|value| value.to_str().unwrap())
    }

    /// The session cookie this reply sets: its value, and the whole header.
    fn session(&self) -> (String, String) {
        let set = self.header("set-cookie").expect("a session cookie").to_string();
        let value = set.split(';').next().unwrap().strip_prefix("spinikus_session=").unwrap().to_string();
        (value, set)
    }

    /// The page's CSRF token, from its sign-out form.
    fn csrf(&self) -> String {
        let start = self.body.find(r#"name="_csrf" value=""#).expect("a CSRF field") + r#"name="_csrf" value=""#.len();
        self.body[start..start + 64].to_string()
    }
}

impl Web {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let pool = connect(&dir.path().join("spinikus.db")).await.unwrap();
        migrate(&pool).await.unwrap();
        let store = SqliteStore::open(pool, &MasterKey::generate()).await.unwrap();
        let control = Control::new(store).with_log_dir(dir.path().join("logs"));
        let router = spinikus_web::router(control.clone());
        Self { _dir: dir, control, router }
    }

    async fn send(&self, method: Method, uri: &str, cookie: Option<&str>, headers: &[(&str, &str)], form: Option<&str>) -> Reply {
        let mut request = Request::builder().method(method).uri(uri).header(header::HOST, "spinikus.test");
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, format!("spinikus_session={cookie}"));
        }
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let request = match form {
            Some(form) => {
                request.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded").body(Body::from(form.to_string()))
            }
            None => request.body(Body::empty()),
        };
        let response = self.router.clone().oneshot(request.unwrap()).await.unwrap();
        let (parts, body) = response.into_parts();
        let body = String::from_utf8(body.collect().await.unwrap().to_bytes().to_vec()).unwrap();
        Reply { status: parts.status, headers: parts.headers, body }
    }

    /// The events of a server-sent event stream until it ends, or until `until` shows up.
    async fn events(&self, uri: &str, cookie: &str, until: &str) -> String {
        let request = Request::builder().uri(uri).header(header::HOST, "spinikus.test");
        let request = request.header(header::COOKIE, format!("spinikus_session={cookie}")).body(Body::empty()).unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/event-stream");
        let mut body = response.into_body();
        let mut text = String::new();
        while !text.contains(until) {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(5), body.frame()).await.expect("an event in time");
            match frame {
                Some(frame) => text.push_str(std::str::from_utf8(&frame.unwrap().into_data().unwrap_or_default()).unwrap()),
                None => break,
            }
        }
        text
    }

    async fn get(&self, uri: &str, cookie: Option<&str>) -> Reply {
        self.send(Method::GET, uri, cookie, &[], None).await
    }

    async fn post(&self, uri: &str, cookie: Option<&str>, form: &str) -> Reply {
        self.send(Method::POST, uri, cookie, &[("sec-fetch-site", "same-origin"), ("origin", "http://spinikus.test")], Some(form))
            .await
    }

    async fn sign_in(&self, email: &str) -> String {
        let reply = self.post("/login", None, &format!("email={email}&password=correct+horse+battery+staple")).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
        reply.session().0
    }

    async fn user(&self, email: &str) {
        let fields = UserFields { email: email.parse().unwrap(), name: "Grace".to_string(), admin: false };
        self.control.store().create_user(fields, Some(password::hash(Secret::new(PASSWORD)).await)).await.unwrap();
    }
}

#[tokio::test]
async fn setup_creates_the_first_admin_and_signs_them_in() {
    let web = Web::new().await;

    let home = web.get("/", None).await;
    let login = web.get("/login", None).await;
    let form = web.get("/setup", None).await;
    let short = web.post("/setup", None, "name=Ada&email=ada%40example.com&password=short").await;
    let created = web.post("/setup", None, "name=Ada&email=ada%40example.com&password=correct+horse+battery+staple").await;
    let (session, set_cookie) = created.session();
    let overview = web.get("/", Some(&session)).await;
    let again = web.get("/setup", None).await;
    let taken = web.post("/setup", None, "name=Eve&email=eve%40example.com&password=correct+horse+battery+staple").await;

    assert_eq!((home.status, home.header("location")), (StatusCode::SEE_OTHER, Some("/login")));
    assert_eq!((login.status, login.header("location")), (StatusCode::SEE_OTHER, Some("/setup")));
    assert!(form.body.contains("Create the first admin"), "{}", form.body);
    assert_eq!(short.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(short.body.contains("use at least 12 characters"), "{}", short.body);
    assert!(short.body.contains(r#"value="ada@example.com""#), "the form keeps what was typed");
    assert_eq!((created.status, created.header("location")), (StatusCode::SEE_OTHER, Some("/")));
    assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Lax") && set_cookie.contains("Path=/"));
    assert!(!set_cookie.contains("Secure"), "plain HTTP without a proxy");
    assert_eq!(overview.status, StatusCode::OK);
    assert!(overview.body.contains("No projects yet"), "{}", overview.body);
    assert_eq!(again.header("location"), Some("/login"));
    assert_eq!(taken.header("location"), Some("/login"));
    assert_eq!(web.control.store().count_users().await.unwrap(), 1);
}

#[tokio::test]
async fn signs_in_and_out() {
    let web = Web::new().await;
    web.control.create_first_admin("ada@example.com", "Ada", Secret::new(PASSWORD)).await.unwrap();

    let wrong = web.post("/login", None, "email=ada%40example.com&password=nope&next=%2Fprojects").await;
    let protected = web.get("/projects/prj_01JAAAAAAAAAAAAAAAAAAAAAAA?tab=1", None).await;
    let signed_in =
        web.post("/login", None, "email=ada%40example.com&password=correct+horse+battery+staple&next=%2Fprojects%2Fx").await;
    let elsewhere =
        web.post("/login", None, "email=ada%40example.com&password=correct+horse+battery+staple&next=%2F%2Fevil.example").await;
    let behind_https = web
        .send(
            Method::POST,
            "/login",
            None,
            &[("x-forwarded-proto", "https")],
            Some("email=ada%40example.com&password=correct+horse+battery+staple"),
        )
        .await;
    let (session, _) = signed_in.session();
    let login_again = web.get("/login?next=/projects", Some(&session)).await;
    let overview = web.get("/", Some(&session)).await;
    let without_token = web.post("/logout", Some(&session), "").await;
    let signed_out = web.post("/logout", Some(&session), &format!("_csrf={}", overview.csrf())).await;
    let after = web.get("/", Some(&session)).await;
    let htmx = web.send(Method::GET, "/", Some(&session), &[("hx-request", "true")], None).await;
    let boosted_form = [("hx-request", "true"), ("sec-fetch-site", "same-origin")];
    let form = "email=ada%40example.com&password=correct+horse+battery+staple&next=%2Fprojects%2Fx";
    let boosted = web.send(Method::POST, "/login", None, &boosted_form, Some(form)).await;

    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert!(wrong.body.contains("Wrong email or password"));
    assert!(wrong.body.contains(r#"name="next" value="/projects""#), "{}", wrong.body);
    assert_eq!(protected.header("location"), Some("/login?next=/projects/prj_01JAAAAAAAAAAAAAAAAAAAAAAA%3Ftab%3D1"));
    assert_eq!(signed_in.header("location"), Some("/projects/x"));
    assert_eq!(elsewhere.header("location"), Some("/"));
    assert!(behind_https.session().1.contains("; Secure"));
    assert_eq!(login_again.header("location"), Some("/projects"));
    assert!(overview.body.contains("Sign out") && overview.body.contains("Ada"));
    assert_eq!(without_token.status, StatusCode::FORBIDDEN);
    assert_eq!((signed_out.status, signed_out.header("location")), (StatusCode::SEE_OTHER, Some("/login")));
    assert!(signed_out.header("set-cookie").unwrap().contains("Max-Age=0"));
    assert_eq!(after.header("location"), Some("/login"), "the session is gone");
    assert_eq!((htmx.status, htmx.header("hx-redirect")), (StatusCode::UNAUTHORIZED, Some("/login")));
    assert_eq!((boosted.status, boosted.header("hx-location")), (StatusCode::OK, Some("/projects/x")));
    assert!(boosted.header("set-cookie").unwrap().starts_with("spinikus_session="));
}

#[tokio::test]
async fn refuses_changes_from_other_sites() {
    let web = Web::new().await;
    web.control.create_first_admin("ada@example.com", "Ada", Secret::new(PASSWORD)).await.unwrap();
    let session = web.sign_in("ada%40example.com").await;
    let token = web.get("/", Some(&session)).await.csrf();
    let form = format!("_csrf={token}");

    let cross_site = web.send(Method::POST, "/logout", Some(&session), &[("sec-fetch-site", "cross-site")], Some(&form)).await;
    let other_origin =
        web.send(Method::POST, "/logout", Some(&session), &[("origin", "https://evil.example")], Some(&form)).await;
    let login_elsewhere = web
        .send(
            Method::POST,
            "/login",
            None,
            &[("origin", "https://evil.example")],
            Some("email=ada%40example.com&password=correct+horse+battery+staple"),
        )
        .await;
    let wrong_token = web.post("/logout", Some(&session), &format!("_csrf={}", "0".repeat(64))).await;
    let header_token = web.send(Method::POST, "/logout", Some(&session), &[("x-csrf-token", &token)], None).await;

    assert_eq!(cross_site.status, StatusCode::FORBIDDEN);
    assert_eq!(other_origin.status, StatusCode::FORBIDDEN);
    assert_eq!(login_elsewhere.status, StatusCode::FORBIDDEN);
    assert_eq!(wrong_token.status, StatusCode::FORBIDDEN);
    assert_eq!(header_token.status, StatusCode::SEE_OTHER, "HTMX sends the token as a header");
}

#[tokio::test]
async fn pages_show_only_what_the_user_may_see() {
    let web = Web::new().await;
    let control = &web.control;
    control.create_first_admin("ada@example.com", "Ada", Secret::new(PASSWORD)).await.unwrap();
    let (_, secret) = control.sign_in("ada@example.com", Secret::new(PASSWORD), Default::default()).await.unwrap();
    let admin = control.authenticate_session(secret.expose()).await.unwrap();
    let project = |name: &str| NewProject { name: name.to_string(), slug: None };
    let shop = control.create_project(&admin, project("Shop")).await.unwrap();
    let secret = control.create_project(&admin, project("Secret plans")).await.unwrap();
    let environment = |name: &str, kind| NewEnvironment { name: name.to_string(), slug: None, kind };
    let production =
        control.create_environment(&admin, shop.id, environment("Production", EnvironmentKind::Production)).await.unwrap();
    let staging = control.create_environment(&admin, shop.id, environment("Staging", EnvironmentKind::Staging)).await.unwrap();
    let service = |name: &str| NewService {
        name: name.to_string(),
        slug: None,
        kind: ServiceKind::App,
        config: ServiceConfig::default(),
        server_id: None,
    };
    let storefront = control.create_service(&admin, production.id, service("Storefront")).await.unwrap();
    control.create_service(&admin, staging.id, service("Staging only")).await.unwrap();
    web.user("grace@example.com").await;
    let grace = control.store().find_user_by_email(&"grace@example.com".parse().unwrap()).await.unwrap().unwrap();
    control.store().create_membership(grace.id, MembershipScope::Environment(production.id), Role::Viewer).await.unwrap();
    let session = web.sign_in("grace%40example.com").await;

    let overview = web.get("/", Some(&session)).await;
    let project_page = web.get(&format!("/projects/{}", shop.id), Some(&session)).await;
    let environment_page = web.get(&format!("/environments/{}", production.id), Some(&session)).await;
    let service_page = web.get(&format!("/services/{}", storefront.id), Some(&session)).await;
    let hidden_project = web.get(&format!("/projects/{}", secret.id), Some(&session)).await;
    let hidden_environment = web.get(&format!("/environments/{}", staging.id), Some(&session)).await;
    let nonsense = web.get("/services/not-an-id", Some(&session)).await;
    let nowhere = web.get("/nowhere", Some(&session)).await;

    assert_eq!(overview.status, StatusCode::OK);
    assert!(overview.body.contains("Shop") && !overview.body.contains("Secret plans"), "{}", overview.body);
    assert!(overview.body.contains("Production") && !overview.body.contains("Staging"));
    assert!(project_page.body.contains("Storefront") && !project_page.body.contains("Staging only"), "{}", project_page.body);
    assert!(environment_page.body.contains("Storefront"));
    assert!(environment_page.body.contains(&format!(r#"href="/projects/{}""#, shop.id)), "breadcrumbs");
    assert_eq!(service_page.status, StatusCode::OK);
    assert!(service_page.body.contains("storefront") && service_page.body.contains("embedded server"));
    for (page, reply) in
        [("project", &hidden_project), ("environment", &hidden_environment), ("nonsense", &nonsense), ("nowhere", &nowhere)]
    {
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{page}");
        assert!(reply.body.contains("Not found"), "{page}: {}", reply.body);
    }
}

#[tokio::test]
async fn serves_assets_for_good_and_pages_never_cached() {
    let web = Web::new().await;
    web.control.create_first_admin("ada@example.com", "Ada", Secret::new(PASSWORD)).await.unwrap();
    let session = web.sign_in("ada%40example.com").await;

    let page = web.get("/", Some(&session)).await;
    let start = page.body.find("/assets/app.css?v=").unwrap();
    let url = &page.body[start..page.body[start..].find('"').unwrap() + start];
    let css = web.get(url, None).await;
    let old = web.get("/assets/app.css?v=0000", None).await;
    let etag = css.header("etag").unwrap().to_string();
    let unchanged = web.send(Method::GET, url, None, &[("if-none-match", &etag)], None).await;
    let htmx = web.get("/assets/htmx.min.js", None).await;
    let missing = web.get("/assets/nope.js", None).await;

    assert_eq!(page.header("cache-control"), Some("no-store"));
    let policy = page.header("content-security-policy").unwrap();
    assert!(policy.contains("script-src 'self'") && policy.contains("frame-ancestors 'none'"), "{policy}");
    assert_eq!(page.header("x-frame-options"), Some("DENY"));
    assert!(page.body.contains(r#"hx-headers="{&quot;X-CSRF-Token&quot;:"#), "HTMX requests carry the CSRF token");
    assert_eq!(css.status, StatusCode::OK);
    assert_eq!(css.header("content-type"), Some("text/css; charset=utf-8"));
    assert_eq!(css.header("cache-control"), Some("public, max-age=31536000, immutable"));
    assert_eq!(old.header("cache-control"), Some("no-cache"));
    assert_eq!(unchanged.status, StatusCode::NOT_MODIFIED);
    assert!(htmx.body.starts_with("var htmx="));
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

/// Drives a deployment live without an agent, as the runner would after the agent's reports.
async fn go_live(control: &Control, id: spinikus_core::id::DeploymentId, image: &str) {
    let store = control.store();
    store.transition_deployment(id, DeploymentStatus::Preparing, None, None, Actor::System).await.unwrap();
    store.record_build(id, None, Some(image.to_string()), None).await.unwrap();
    for status in [DeploymentStatus::Starting, DeploymentStatus::Verifying, DeploymentStatus::Live] {
        store.transition_deployment(id, status, None, None, Actor::System).await.unwrap();
    }
}

#[tokio::test]
async fn deploys_follows_cancels_and_rolls_back() {
    let web = Web::new().await;
    let control = &web.control;
    control.create_first_admin("ada@example.com", "Ada", Secret::new(PASSWORD)).await.unwrap();
    let (_, secret) = control.sign_in("ada@example.com", Secret::new(PASSWORD), Default::default()).await.unwrap();
    let admin = control.authenticate_session(secret.expose()).await.unwrap();
    let shop = control.create_project(&admin, NewProject { name: "Shop".to_string(), slug: None }).await.unwrap();
    let production = NewEnvironment { name: "Production".to_string(), slug: None, kind: EnvironmentKind::Production };
    let production = control.create_environment(&admin, shop.id, production).await.unwrap();
    let server = control.create_server(&admin, NewServer { name: "web-1".to_string() }).await.unwrap();
    let service = |name: &str| NewService {
        name: name.to_string(),
        slug: None,
        kind: ServiceKind::App,
        config: ServiceConfig::default(),
        server_id: Some(server.id),
    };
    let web_app = control.create_service(&admin, production.id, service("Storefront")).await.unwrap();
    let sourceless = control.create_service(&admin, production.id, service("Unfinished")).await.unwrap();
    let image = SourceSpec::Image(ImageSource { image: "nginx:1".to_string() });
    control.create_source(&admin, web_app.id, image).await.unwrap();
    web.user("grace@example.com").await;
    let grace = control.store().find_user_by_email(&"grace@example.com".parse().unwrap()).await.unwrap().unwrap();
    control.store().create_membership(grace.id, MembershipScope::Environment(production.id), Role::Viewer).await.unwrap();
    let session = web.sign_in("ada%40example.com").await;
    let viewer = web.sign_in("grace%40example.com").await;
    let service_url = format!("/services/{}", web_app.id);
    let token = web.get(&service_url, Some(&session)).await.csrf();
    let form = format!("_csrf={token}");
    let viewer_token = web.get(&service_url, Some(&viewer)).await.csrf();

    // Deploying, and following the deployment while it is queued.
    let nothing_to_deploy = web.post(&format!("/services/{}/deploy", sourceless.id), Some(&session), &form).await;
    let by_viewer = web.post(&format!("{service_url}/deploy"), Some(&viewer), &format!("_csrf={viewer_token}")).await;
    let deployed = web.post(&format!("{service_url}/deploy"), Some(&session), &form).await;
    let deployment_url = deployed.header("location").unwrap().to_string();
    let following = web.get(&deployment_url, Some(&session)).await;
    let busy_service = web.get(&service_url, Some(&session)).await;
    let viewer_service = web.get(&service_url, Some(&viewer)).await;
    let queued_events = web.events(&format!("{deployment_url}/stream"), &session, "event: status").await;
    let summary = web.get(&format!("{deployment_url}/summary"), Some(&session)).await;

    assert_eq!(nothing_to_deploy.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(nothing_to_deploy.body.contains("The service has no source to deploy."), "{}", nothing_to_deploy.body);
    assert_eq!(by_viewer.status, StatusCode::FORBIDDEN);
    assert_eq!(deployed.status, StatusCode::SEE_OTHER);
    assert!(deployment_url.starts_with("/deployments/dep_"), "{deployment_url}");
    assert!(following.body.contains(&format!(r#"sse-connect="{deployment_url}/stream""#)), "{}", following.body);
    assert!(following.body.contains(r#"sse-swap="log""#) && following.body.contains("queued"));
    assert!(following.body.contains("/cancel"), "developers can cancel");
    assert!(busy_service.body.contains(r#"hx-trigger="every 2s""#), "the history refreshes while one is in progress");
    assert!(busy_service.body.contains(r#"/deploy""#));
    assert!(!viewer_service.body.contains(r#"/deploy""#), "viewers can't deploy");
    assert!(queued_events.contains("event: status\ndata: queued"), "{queued_events}");
    assert!(summary.body.starts_with(r#"<section id="summary""#) && summary.body.contains(r#"hx-trigger="sse:status""#));

    // Canceling ends it; the finished page shows its log and follows nothing.
    let canceled = web.post(&format!("{deployment_url}/cancel"), Some(&session), &form).await;
    let again = web.post(&format!("{deployment_url}/cancel"), Some(&session), &form).await;
    let finished = web.get(&deployment_url, Some(&session)).await;
    let ended_events = web.events(&format!("{deployment_url}/stream"), &session, "event: end").await;
    let resumed =
        web.send(Method::GET, &format!("{deployment_url}/stream"), Some(&session), &[("last-event-id", "100000")], None).await;

    assert_eq!((canceled.status, canceled.header("location")), (StatusCode::SEE_OTHER, Some(deployment_url.as_str())));
    assert_eq!(again.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(again.body.contains("cannot move from"), "{}", again.body);
    assert!(finished.body.contains("canceled_by_user") && !finished.body.contains("sse-connect"), "{}", finished.body);
    assert!(finished.body.contains("==&gt; canceled"), "the log is in the page: {}", finished.body);
    assert!(ended_events.contains("event: log") && ended_events.contains("==&gt; canceled"), "{ended_events}");
    assert!(ended_events.contains("event: end\ndata: canceled"), "{ended_events}");
    assert!(!resumed.body.contains("event: log"), "nothing after the offset: {}", resumed.body);

    // Rolling back to a replaced deployment.
    let first = control.create_deployment(&admin, web_app.id).await.unwrap();
    go_live(control, first.id, "nginx@sha256:1").await;
    let second = control.create_deployment(&admin, web_app.id).await.unwrap();
    go_live(control, second.id, "nginx@sha256:2").await;
    control.store().transition_deployment(first.id, DeploymentStatus::Superseded, None, None, Actor::System).await.unwrap();
    let history = web.get(&service_url, Some(&session)).await;
    let rollback_form = format!("_csrf={token}&deployment_id={}", first.id);
    let rolled_back = web.post(&format!("{service_url}/rollback"), Some(&session), &rollback_form).await;
    let rollback = web.get(rolled_back.header("location").unwrap(), Some(&session)).await;
    let no_target = web.post(&format!("/services/{}/rollback", sourceless.id), Some(&session), &form).await;

    assert!(history.body.contains("nginx@sha256:2") && history.body.contains("Running now"), "{}", history.body);
    assert_eq!(history.body.matches(r#"name="deployment_id""#).count(), 1, "only the replaced one can be rolled back to");
    assert_eq!(rolled_back.status, StatusCode::SEE_OTHER);
    assert!(rollback.body.contains("Rolls back to") && rollback.body.contains(&first.id.to_string()), "{}", rollback.body);
    assert_eq!(no_target.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(no_target.body.contains("The service has no earlier deployment to roll back to."), "{}", no_target.body);
}

/// A form body from its fields.
fn form(fields: &[(&str, &str)]) -> String {
    let encode = |text: &str| {
        text.bytes()
            .map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (byte as char).to_string(),
                _ => format!("%{byte:02X}"),
            })
            .collect::<String>()
    };
    fields.iter().map(|(name, value)| format!("{name}={}", encode(value))).collect::<Vec<_>>().join("&")
}

/// The ID in a 303's `Location` after `prefix`, such as `/projects/`.
fn created(reply: &Reply, prefix: &str) -> String {
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    let location = reply.header("location").unwrap();
    location.strip_prefix(prefix).unwrap_or_else(|| panic!("{location} under {prefix}")).to_string()
}

#[tokio::test]
async fn creates_edits_and_deletes_projects_environments_and_services() {
    let web = Web::new().await;
    let control = &web.control;
    control.create_first_admin("ada@example.com", "Ada", Secret::new(PASSWORD)).await.unwrap();
    let (_, secret) = control.sign_in("ada@example.com", Secret::new(PASSWORD), Default::default()).await.unwrap();
    let admin = control.authenticate_session(secret.expose()).await.unwrap();
    let session = web.sign_in("ada%40example.com").await;
    let token = web.get("/", Some(&session)).await.csrf();
    let post = |uri: String, fields: Vec<(&'static str, String)>| {
        let mut all = vec![("_csrf", token.clone())];
        all.extend(fields);
        let body = form(&all.iter().map(|(name, value)| (*name, value.as_str())).collect::<Vec<_>>());
        let session = session.clone();
        let web = &web;
        async move { web.post(&uri, Some(&session), &body).await }
    };
    let field = |name: &'static str, value: &str| (name, value.to_string());

    // A project, an environment in it, and a service with an image and a port.
    assert!(web.get("/", Some(&session)).await.body.contains(r#"href="/projects/new""#));
    assert_eq!(web.get("/projects/new", Some(&session)).await.status, StatusCode::OK);
    let refused = post("/projects".into(), vec![field("name", ""), field("slug", "")]).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", refused.body);
    let project_id = created(&post("/projects".into(), vec![field("name", "Shop"), field("slug", "")]).await, "/projects/");
    let project_page = web.get(&format!("/projects/{project_id}"), Some(&session)).await;
    assert!(project_page.body.contains("Shop") && project_page.body.contains("New environment"), "{}", project_page.body);
    let environments = format!("/projects/{project_id}/environments");
    let fields = vec![field("name", "Production"), field("slug", ""), field("kind", "production")];
    let environment_id = created(&post(environments, fields).await, "/environments/");
    let environment_page = web.get(&format!("/environments/{environment_id}"), Some(&session)).await;
    assert!(environment_page.body.contains("New service"), "{}", environment_page.body);
    assert_eq!(web.get(&format!("/environments/{environment_id}/services/new"), Some(&session)).await.status, StatusCode::OK);
    let services = format!("/environments/{environment_id}/services");
    let new_service = |port: &str| {
        vec![
            field("name", "Storefront"),
            field("slug", ""),
            field("source_type", "image"),
            field("image", "nginx:1.27"),
            field("repo_url", ""),
            field("branch", "main"),
            field("root_directory", ""),
            field("port", port),
        ]
    };
    let bad_port = post(services.clone(), new_service("http")).await;
    assert_eq!(bad_port.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(bad_port.body.contains("The port must be a number") && bad_port.body.contains("nginx:1.27"), "{}", bad_port.body);
    assert_eq!(bad_port.header("hx-push-url"), None, "only for HTMX");
    let boosted = [("sec-fetch-site", "same-origin"), ("hx-request", "true"), ("x-csrf-token", token.as_str())];
    let body = form(&[("name", ""), ("slug", "")]);
    let refused = web.send(Method::POST, "/projects", Some(&session), &boosted, Some(&body)).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(refused.header("hx-push-url"), Some("false"), "the address bar keeps the form's page");
    let service_id = created(&post(services.clone(), new_service("8080")).await, "/services/");
    let id: spinikus_core::id::ServiceId = service_id.parse().unwrap();
    let service = control.get_service(&admin, id).await.unwrap();
    assert_eq!(service.slug.to_string(), "storefront");
    assert_eq!(service.config.get("runtime"), Some(&serde_json::json!({ "port": 8080 })));
    let source = control.get_source(&admin, id).await.unwrap();
    assert_eq!(source.spec, SourceSpec::Image(ImageSource { image: "nginx:1.27".to_string() }));
    // A source that doesn't check out leaves no service behind.
    let mut no_image = new_service("");
    no_image[0] = field("name", "Broken");
    no_image[3] = field("image", "");
    assert_eq!(post(services, no_image).await.status, StatusCode::UNPROCESSABLE_ENTITY);
    let listed = control.list_services(&admin, environment_id.parse().unwrap(), None, 10).await.unwrap();
    assert_eq!(listed.items.len(), 1);

    // The settings: renaming, a Git source over SSH with its deploy key, and runtime settings
    // that keep what the form doesn't edit.
    let settings = format!("/services/{service_id}/settings");
    assert!(web.get(&format!("/services/{service_id}"), Some(&session)).await.body.contains(&format!(r#"href="{settings}""#)));
    let page = web.get(&settings, Some(&session)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Delete service") && !page.body.contains("Deploy key"), "{}", page.body);
    let revision = service.revision.to_string();
    let renamed = post(
        format!("{settings}/general"),
        vec![field("name", "Shopfront"), field("slug", "shop"), field("revision", &revision)],
    )
    .await;
    assert_eq!(renamed.header("location"), Some(format!("{settings}?saved=general").as_str()));
    let stale =
        post(format!("{settings}/general"), vec![field("name", "Again"), field("slug", ""), field("revision", &revision)]).await;
    assert_eq!(stale.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(stale.body.contains("Someone changed this while you were editing"), "{}", stale.body);
    let git = vec![
        field("source_type", "git"),
        field("image", ""),
        field("repo_url", "git@github.com:acme/shop.git"),
        field("branch", "release"),
        field("root_directory", "apps/web"),
        field("source_revision", &source.revision.to_string()),
    ];
    assert_eq!(post(format!("{settings}/source"), git).await.status, StatusCode::SEE_OTHER);
    let page = web.get(&format!("{settings}?saved=source"), Some(&session)).await;
    assert!(
        page.body.contains("Saved.") && page.body.contains("Deploy key") && page.body.contains("ssh-ed25519 "),
        "{}",
        page.body
    );
    assert!(page.body.contains(r#"value="git" checked"#) && page.body.contains("apps/web"), "{}", page.body);

    let mut config = control.get_service(&admin, id).await.unwrap().config;
    config.insert("runtime".to_string(), serde_json::json!({ "port": 8080, "healthcheck": { "path": "/up", "timeout": "5s" } }));
    let service = control
        .update_service(
            &admin,
            id,
            service.revision + 1,
            spinikus_control::ServiceChanges { config: Some(config), ..Default::default() },
        )
        .await
        .unwrap();
    let runtime = |port: &str, revision: i64| {
        vec![
            field("port", port),
            field("start", "./server --listen 0.0.0.0"),
            field("health_path", "/healthz"),
            field("start_period", ""),
            field("shutdown_timeout", "20s"),
            field("revision", &revision.to_string()),
        ]
    };
    let refused = post(format!("{settings}/runtime"), runtime("70000", service.revision)).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(refused.body.contains("./server --listen"), "keeps what was typed");
    let invalid = post(format!("{settings}/runtime"), {
        let mut fields = runtime("3000", service.revision);
        fields[4] = field("shutdown_timeout", "soon");
        fields
    })
    .await;
    assert_eq!(invalid.status, StatusCode::UNPROCESSABLE_ENTITY, "the control layer checks durations: {}", invalid.body);
    assert_eq!(post(format!("{settings}/runtime"), runtime("3000", service.revision)).await.status, StatusCode::SEE_OTHER);
    let service = control.get_service(&admin, id).await.unwrap();
    assert_eq!(
        service.config.get("runtime"),
        Some(&serde_json::json!({
            "port": 3000,
            "start": "./server --listen 0.0.0.0",
            "shutdown_timeout": "20s",
            "healthcheck": { "path": "/healthz", "timeout": "5s" },
        }))
    );

    // A viewer gets no buttons and no settings.
    web.user("grace@example.com").await;
    let grace = control.store().find_user_by_email(&"grace@example.com".parse().unwrap()).await.unwrap().unwrap();
    let environment: spinikus_core::id::EnvironmentId = environment_id.parse().unwrap();
    control.store().create_membership(grace.id, MembershipScope::Environment(environment), Role::Viewer).await.unwrap();
    let viewer = web.sign_in("grace%40example.com").await;
    assert!(!web.get("/", Some(&viewer)).await.body.contains("New project"));
    assert!(!web.get(&format!("/environments/{environment_id}"), Some(&viewer)).await.body.contains("New service"));
    assert!(!web.get(&format!("/services/{service_id}"), Some(&viewer)).await.body.contains("Settings"));
    assert_eq!(web.get(&settings, Some(&viewer)).await.status, StatusCode::FORBIDDEN);
    assert_eq!(web.get(&format!("/projects/{project_id}/settings"), Some(&viewer)).await.status, StatusCode::FORBIDDEN);
    assert_eq!(web.get("/projects/new", Some(&viewer)).await.status, StatusCode::FORBIDDEN);
    let viewer_token = web.get("/", Some(&viewer)).await.csrf();
    let delete = web.post(&format!("/services/{service_id}/delete"), Some(&viewer), &format!("_csrf={viewer_token}")).await;
    assert_eq!(delete.status, StatusCode::FORBIDDEN);

    // Renaming the project and the environment; deleting refuses parents with children.
    let project = control.get_project(&admin, project_id.parse().unwrap()).await.unwrap();
    let renamed = post(
        format!("/projects/{project_id}/settings"),
        vec![field("name", "Shop 2"), field("slug", ""), field("revision", &project.revision.to_string())],
    )
    .await;
    assert_eq!(renamed.status, StatusCode::SEE_OTHER);
    assert_eq!(control.get_project(&admin, project.id).await.unwrap().slug.to_string(), "shop", "kept when empty");
    let staging = post(
        format!("/environments/{environment_id}/settings"),
        vec![field("name", "Live"), field("slug", "live"), field("revision", "1")],
    )
    .await;
    assert_eq!(staging.status, StatusCode::SEE_OTHER, "{}", staging.body);
    let refused = post(format!("/projects/{project_id}/delete"), vec![]).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(refused.body.contains("Delete the project's environments first."));
    let refused = post(format!("/environments/{environment_id}/delete"), vec![]).await;
    assert!(refused.body.contains("Delete the environment's services first."), "{}", refused.body);
    let deleted = post(format!("/services/{service_id}/delete"), vec![]).await;
    assert_eq!(deleted.header("location"), Some(format!("/environments/{environment_id}").as_str()));
    let deleted = post(format!("/environments/{environment_id}/delete"), vec![]).await;
    assert_eq!(deleted.header("location"), Some(format!("/projects/{project_id}").as_str()));
    let deleted = post(format!("/projects/{project_id}/delete"), vec![]).await;
    assert_eq!(deleted.header("location"), Some("/"));
    assert_eq!(web.get(&format!("/projects/{project_id}"), Some(&session)).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn runtime_settings_from_the_repository_are_read_only() {
    let web = Web::new().await;
    let control = &web.control;
    control.create_first_admin("ada@example.com", "Ada", Secret::new(PASSWORD)).await.unwrap();
    let (_, secret) = control.sign_in("ada@example.com", Secret::new(PASSWORD), Default::default()).await.unwrap();
    let admin = control.authenticate_session(secret.expose()).await.unwrap();
    let shop = control.create_project(&admin, NewProject { name: "Shop".to_string(), slug: None }).await.unwrap();
    let production = NewEnvironment { name: "Production".to_string(), slug: None, kind: EnvironmentKind::Production };
    let production = control.create_environment(&admin, shop.id, production).await.unwrap();
    let server = control.create_server(&admin, NewServer { name: "web-1".to_string() }).await.unwrap();
    let mut config = ServiceConfig::new();
    config.insert("runtime".to_string(), serde_json::json!({ "port": 3000, "start": "./old" }));
    let new = NewService { name: "Web".to_string(), slug: None, kind: ServiceKind::App, config, server_id: Some(server.id) };
    let git =
        GitSource { repo_url: "https://github.com/acme/web.git".to_string(), branch: "main".to_string(), root_directory: None };
    let service = control.create_service_with_source(&admin, production.id, new, SourceSpec::Git(git)).await.unwrap();
    let session = web.sign_in("ada%40example.com").await;
    let settings = format!("/services/{}/settings", service.id);
    let page = web.get(&settings, Some(&session)).await;
    assert!(!page.body.contains("Set by spinikus.json"), "nothing built yet: {}", page.body);

    // The build read a spinikus.json that sets the port.
    let deployment = control.create_deployment(&admin, service.id).await.unwrap();
    let mut snapshot = deployment.snapshot.clone();
    snapshot.config.insert("runtime".to_string(), serde_json::json!({ "port": 4000, "start": "./old" }));
    snapshot.from_file = vec!["runtime.port".to_string()];
    let store = control.store();
    store.transition_deployment(deployment.id, DeploymentStatus::Preparing, None, None, Actor::System).await.unwrap();
    store.record_build(deployment.id, Some("a".repeat(40)), Some("sha256:abc".to_string()), Some(snapshot)).await.unwrap();

    let page = web.get(&settings, Some(&session)).await;
    assert!(page.body.contains("Set by spinikus.json"), "{}", page.body);
    let port = page.body.find(r#"id="runtime-port""#).unwrap();
    let input = &page.body[page.body[..port].rfind('<').unwrap()..port + page.body[port..].find('>').unwrap()];
    assert!(input.contains(r#"value="4000""#) && input.contains("readonly"), "{input}");
    let start = page.body.find(r#"id="runtime-start""#).unwrap();
    assert!(!page.body[start..start + page.body[start..].find('>').unwrap()].contains("readonly"));

    let token = page.csrf();
    let fields = [
        ("_csrf", token.as_str()),
        ("port", "9999"),
        ("start", "./new"),
        ("health_path", ""),
        ("start_period", ""),
        ("shutdown_timeout", ""),
        ("revision", &service.revision.to_string()),
    ];
    let saved = web.post(&format!("{settings}/runtime"), Some(&session), &form(&fields)).await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER, "{}", saved.body);
    let service = control.get_service(&admin, service.id).await.unwrap();
    assert_eq!(
        service.config.get("runtime"),
        Some(&serde_json::json!({ "port": 3000, "start": "./new" })),
        "the stored port is kept"
    );
}

/// An instance admin, a project with a production environment and an app service in it, and
/// two more users: `dev@example.com`, a developer of the environment, and `viewer@example.com`,
/// a viewer of it. Returns the admin's principal and the IDs.
async fn shop_with_team(
    web: &Web,
) -> (
    spinikus_control::Principal,
    spinikus_core::project::Project,
    spinikus_core::environment::Environment,
    spinikus_core::service::Service,
) {
    let control = &web.control;
    control.create_first_admin("ada@example.com", "Ada", Secret::new(PASSWORD)).await.unwrap();
    let (_, secret) = control.sign_in("ada@example.com", Secret::new(PASSWORD), Default::default()).await.unwrap();
    let admin = control.authenticate_session(secret.expose()).await.unwrap();
    let shop = control.create_project(&admin, NewProject { name: "Shop".to_string(), slug: None }).await.unwrap();
    let production = NewEnvironment { name: "Production".to_string(), slug: None, kind: EnvironmentKind::Production };
    let production = control.create_environment(&admin, shop.id, production).await.unwrap();
    let new =
        NewService { name: "Web".to_string(), slug: None, kind: ServiceKind::App, config: ServiceConfig::new(), server_id: None };
    let service = control.create_service(&admin, production.id, new).await.unwrap();
    for (email, role) in [("dev@example.com", Role::Developer), ("viewer@example.com", Role::Viewer)] {
        web.user(email).await;
        let user = control.store().find_user_by_email(&email.parse().unwrap()).await.unwrap().unwrap();
        control.store().create_membership(user.id, MembershipScope::Environment(production.id), role).await.unwrap();
    }
    (admin, shop, production, service)
}

#[tokio::test]
async fn manages_variables_on_every_scope_and_reveals_secrets_only_to_admins() {
    use spinikus_control::NewVariable;
    use spinikus_core::variable::{VariableAvailability, VariableScope};

    let web = Web::new().await;
    let control = &web.control;
    let (admin, shop, production, service) = shop_with_team(&web).await;
    let variable = |name: &str, value: &str| NewVariable {
        name: name.to_string(),
        value: Secret::new(value),
        availability: VariableAvailability::Runtime,
        secret: false,
    };
    control.create_variable(&admin, VariableScope::Project(shop.id), variable("REGION", "eu-west")).await.unwrap();
    control.create_variable(&admin, VariableScope::Environment(production.id), variable("API_URL", "https://old")).await.unwrap();
    let ada = web.sign_in("ada%40example.com").await;
    let dev = web.sign_in("dev%40example.com").await;
    let viewer = web.sign_in("viewer%40example.com").await;
    let page = format!("/services/{}/variables", service.id);
    let token = web.get(&page, Some(&dev)).await.csrf();
    let add = |name: &str, value: &str, secret: bool| {
        let mut fields = vec![("_csrf", token.as_str()), ("name", name), ("value", value), ("availability", "runtime")];
        if secret {
            fields.push(("secret", "on"));
        }
        form(&fields)
    };

    // A developer adds variables; the browser's CRLF line breaks are stored as LF.
    let added = web.post(&page, Some(&dev), &add("API_URL", "https://new\r\nsecond line", false)).await;
    assert_eq!(added.header("location"), Some(page.as_str()), "{}", added.body);
    let added = web.post(&page, Some(&dev), &add("TOKEN", "hunter2-very-secret", true)).await;
    assert_eq!(added.status, StatusCode::SEE_OTHER);
    let refused = web.post(&page, Some(&dev), &add("1BAD", "kept-value", false)).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(refused.body.contains("cannot start with a digit") && refused.body.contains("kept-value"), "{}", refused.body);
    let variables = control.list_variables(&admin, VariableScope::Service(service.id)).await.unwrap();
    let api_url = variables.iter().find(|variable| variable.name.as_str() == "API_URL").unwrap();
    let token_variable = variables.iter().find(|variable| variable.name.as_str() == "TOKEN").unwrap();
    assert_eq!(api_url.value.as_deref(), Some("https://new\nsecond line"));
    assert!(token_variable.secret);

    // The page: its own variables, the environment's (one overridden here), and the project's
    // only for those with a role in the project. Secret values never show.
    let for_dev = web.get(&page, Some(&dev)).await;
    let for_admin = web.get(&page, Some(&ada)).await;
    let for_viewer = web.get(&page, Some(&viewer)).await;
    for reply in [&for_dev, &for_admin, &for_viewer] {
        assert_eq!(reply.status, StatusCode::OK);
        assert!(!reply.body.contains("hunter2"), "{}", reply.body);
        assert!(reply.body.contains("https://new") && reply.body.contains("overridden here"), "{}", reply.body);
    }
    assert!(for_admin.body.contains("eu-west") && !for_dev.body.contains("eu-west"));
    let reveal = format!("/variables/{}/reveal", token_variable.id);
    assert!(for_admin.body.contains(&reveal) && !for_dev.body.contains(&reveal));
    assert!(for_dev.body.contains("Add a variable") && !for_viewer.body.contains("Add a variable"));
    assert!(!for_viewer.body.contains("/edit"));

    // Revealing: an admin's HTMX request gets the value alone, a plain one the page with it.
    let admin_token = for_admin.csrf();
    let htmx = [("sec-fetch-site", "same-origin"), ("hx-request", "true"), ("x-csrf-token", admin_token.as_str())];
    let revealed = web.send(Method::POST, &reveal, Some(&ada), &htmx, Some("")).await;
    assert_eq!(revealed.status, StatusCode::OK);
    assert!(revealed.body.contains("hunter2-very-secret") && !revealed.body.contains("<html"), "{}", revealed.body);
    let revealed = web.post(&reveal, Some(&ada), &format!("_csrf={admin_token}")).await;
    assert!(revealed.body.contains("hunter2-very-secret") && revealed.body.contains("<html"));
    assert_eq!(web.post(&reveal, Some(&dev), &format!("_csrf={token}")).await.status, StatusCode::FORBIDDEN);

    // Editing a secret: its value isn't on the form, and an empty one keeps it.
    let edit = format!("/variables/{}/edit", token_variable.id);
    let edit_page = web.get(&edit, Some(&dev)).await;
    assert_eq!(edit_page.status, StatusCode::OK);
    assert!(!edit_page.body.contains("hunter2") && edit_page.body.contains("disabled"), "{}", edit_page.body);
    let revision = token_variable.revision.to_string();
    let fields =
        [("_csrf", token.as_str()), ("name", "API_TOKEN"), ("value", ""), ("availability", "both"), ("revision", &revision)];
    let saved = web.post(&edit, Some(&dev), &form(&fields)).await;
    assert_eq!(saved.header("location"), Some(page.as_str()), "{}", saved.body);
    let stale = web.post(&edit, Some(&dev), &form(&fields)).await;
    assert_eq!(stale.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(stale.body.contains("Someone changed this"));
    let updated = control.get_variable(&admin, token_variable.id).await.unwrap();
    assert_eq!((updated.name.as_str(), updated.availability, updated.secret), ("API_TOKEN", VariableAvailability::Both, true));
    assert_eq!(control.reveal_variable(&admin, updated.id).await.unwrap().expose(), "hunter2-very-secret");

    // Deleting; viewers can do neither.
    let delete = format!("/variables/{}/delete", api_url.id);
    let viewer_token = for_viewer.csrf();
    assert_eq!(web.post(&delete, Some(&viewer), &format!("_csrf={viewer_token}")).await.status, StatusCode::FORBIDDEN);
    assert_eq!(
        web.post(&page, Some(&viewer), &add("NOPE", "x", false).replace(&token, &viewer_token)).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(web.post(&delete, Some(&dev), &format!("_csrf={token}")).await.status, StatusCode::SEE_OTHER);
    assert_eq!(control.list_variables(&admin, VariableScope::Service(service.id)).await.unwrap().len(), 1);

    // The environment's and the project's pages; the project's needs a role in the project.
    let environment_page = web.get(&format!("/environments/{}/variables", production.id), Some(&dev)).await;
    assert!(environment_page.body.contains("https://old") && environment_page.body.contains("Add a variable"));
    assert!(environment_page.body.contains(&format!(r#"href="/environments/{}/variables""#, production.id)), "a tab");
    assert_eq!(web.get(&format!("/projects/{}/variables", shop.id), Some(&dev)).await.status, StatusCode::FORBIDDEN);
    let project_page = web.get(&format!("/projects/{}/variables", shop.id), Some(&ada)).await;
    assert!(project_page.body.contains("eu-west"), "{}", project_page.body);
}

#[tokio::test]
async fn adds_redirects_and_removes_domains() {
    let web = Web::new().await;
    let control = &web.control;
    let (admin, _, production, service) = shop_with_team(&web).await;
    let ada = web.sign_in("ada%40example.com").await;
    let viewer = web.sign_in("viewer%40example.com").await;
    let page = format!("/services/{}/domains", service.id);
    let reply = web.get(&page, Some(&ada)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("web-production-shop.localhost") && reply.body.contains("HTTP only"), "{}", reply.body);
    let token = reply.csrf();

    let add =
        |hostname: &str, redirect_to: &str| form(&[("_csrf", &token), ("hostname", hostname), ("redirect_to", redirect_to)]);
    assert_eq!(web.post(&page, Some(&ada), &add("Example.com", "")).await.status, StatusCode::SEE_OTHER);
    let apex = control.list_domains(&admin, service.id).await.unwrap().domains.remove(0);
    assert_eq!(apex.hostname.to_string(), "example.com");
    assert_eq!(web.post(&page, Some(&ada), &add("www.example.com", &apex.id.to_string())).await.status, StatusCode::SEE_OTHER);
    let refused = web.post(&page, Some(&ada), &add("not a host", "")).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(refused.body.contains("not a host") && refused.body.contains(r#"role="alert""#), "{}", refused.body);
    let reserved = web.post(&page, Some(&ada), &add("mine.localhost", "")).await;
    assert_eq!(reserved.status, StatusCode::UNPROCESSABLE_ENTITY);

    let reply = web.get(&page, Some(&ada)).await;
    assert!(reply.body.contains("redirects to") && reply.body.contains("No certificate yet"), "{}", reply.body);
    let www = control
        .list_domains(&admin, service.id)
        .await
        .unwrap()
        .domains
        .into_iter()
        .find(|domain| domain.redirect_to.is_some())
        .unwrap();
    assert_eq!(www.redirect_to, Some(apex.id));

    // Serving the app again, then a stale form.
    let serve = form(&[("_csrf", &token), ("redirect_to", ""), ("revision", &www.revision.to_string())]);
    assert_eq!(web.post(&format!("/domains/{}", www.id), Some(&ada), &serve).await.status, StatusCode::SEE_OTHER);
    assert_eq!(control.get_domain(&admin, www.id).await.unwrap().redirect_to, None);
    let stale = web.post(&format!("/domains/{}", www.id), Some(&ada), &serve).await;
    assert_eq!(stale.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(stale.body.contains("Someone changed this"), "{}", stale.body);

    // A service with domains can't be deleted; viewers see the domains but can't change them.
    let refused = web.post(&format!("/services/{}/delete", service.id), Some(&ada), &format!("_csrf={token}")).await;
    assert!(refused.body.contains("Remove the service's domains first."), "{}", refused.body);
    let for_viewer = web.get(&page, Some(&viewer)).await;
    assert!(for_viewer.body.contains("www.example.com") && !for_viewer.body.contains("Add a domain"));
    let viewer_token = for_viewer.csrf();
    let remove = format!("/domains/{}/delete", www.id);
    assert_eq!(web.post(&remove, Some(&viewer), &format!("_csrf={viewer_token}")).await.status, StatusCode::FORBIDDEN);
    assert_eq!(web.post(&remove, Some(&ada), &format!("_csrf={token}")).await.status, StatusCode::SEE_OTHER);
    assert_eq!(control.list_domains(&admin, service.id).await.unwrap().domains.len(), 1);

    // Databases have no domains, so no tab.
    let new = NewService {
        name: "Cache".to_string(),
        slug: None,
        kind: ServiceKind::Database,
        config: ServiceConfig::new(),
        server_id: None,
    };
    let cache = control.create_service(&admin, production.id, new).await.unwrap();
    let cache_page = web.get(&format!("/services/{}", cache.id), Some(&ada)).await;
    assert!(!cache_page.body.contains("/domains") && cache_page.body.contains("/variables"), "{}", cache_page.body);
    assert!(web.get(&format!("/services/{}/domains", cache.id), Some(&ada)).await.body.contains("Only apps have domains"));
}
