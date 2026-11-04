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
    crash::CrashReason,
    deployment::DeploymentStatus,
    environment::EnvironmentKind,
    membership::{MembershipScope, Role},
    secret::Secret,
    service::{ServiceConfig, ServiceKind},
    source::{GitSource, ImageSource, SourceSpec},
    user::UserFields,
};
use spinikus_store::{
    CrashLoopRepository, DeploymentRepository, MasterKey, MembershipRepository, NewCrashLoop, ServiceRepository, SqliteStore,
    UserRepository, connect, migrate,
};
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
        self.sign_in_with(email, "correct+horse+battery+staple").await
    }

    /// Signs in with a URL-encoded email and password.
    async fn sign_in_with(&self, email: &str, password: &str) -> String {
        let reply = self.post("/login", None, &format!("email={email}&password={password}")).await;
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
    assert!(!overview.body.contains("Needs attention"), "nothing does: {}", overview.body);
    assert_eq!(again.header("location"), Some("/login"));
    assert_eq!(taken.header("location"), Some("/login"));
    assert_eq!(web.control.store().count_users().await.unwrap(), 1);
}

#[tokio::test]
async fn setup_needs_the_setup_code_when_there_is_one() {
    let mut web = Web::new().await;
    let code = Control::new_setup_code();
    web.router = spinikus_web::router(web.control.clone().with_setup_code(Secret::new(code.expose())));

    let form = web.get(&format!("/setup?code={}", code.expose()), None).await;
    assert!(form.body.contains("Setup code") && form.body.contains(&format!(r#"value="{}""#, code.expose())), "{}", form.body);
    let fields = "name=Ada&email=ada%40example.com&password=correct+horse+battery+staple";
    let wrong = web.post("/setup", None, &format!("setup_code=nope&{fields}")).await;
    assert_eq!(wrong.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(wrong.body.contains("The setup code is not valid"), "{}", wrong.body);
    assert!(!wrong.body.contains(code.expose()), "the right code is never shown for a wrong one");
    assert_eq!(web.post("/setup", None, fields).await.status, StatusCode::UNPROCESSABLE_ENTITY, "no code at all");
    let created = web.post("/setup", None, &format!("setup_code={}&{fields}", code.expose())).await;
    assert_eq!((created.status, created.header("location")), (StatusCode::SEE_OTHER, Some("/")));
}

#[tokio::test]
async fn makes_sign_ins_wait_after_too_many_wrong_passwords() {
    let web = Web::new().await;
    web.control.create_first_admin("ada@example.com", "Ada", Secret::new(PASSWORD)).await.unwrap();

    for _ in 0..spinikus_control::throttle::MAX_FAILURES {
        let wrong = web.post("/login", None, "email=ada%40example.com&password=nope").await;
        assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    }
    let throttled = web.post("/login", None, "email=ada%40example.com&password=correct+horse+battery+staple").await;

    assert_eq!(throttled.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(throttled.body.contains("Too many failed sign-ins; try again in 15 minutes."), "{}", throttled.body);
    assert!(throttled.header("set-cookie").is_none());
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
    let environment = |name: &str, kind| NewEnvironment { name: name.to_string(), slug: None, kind, previews: false };
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
    let production =
        NewEnvironment { name: "Production".to_string(), slug: None, kind: EnvironmentKind::Production, previews: false };
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

    // Restarting what runs: offered only with something live. Here no agent has reported a
    // container for it, so the restart is refused; the control plane's tests restart one.
    let restart = web.post(&format!("{service_url}/restart"), Some(&session), &form).await;
    let restarted = web.get(&format!("{service_url}?restarted=1"), Some(&session)).await;
    let nothing_live = web.get(&format!("/services/{}", sourceless.id), Some(&session)).await;

    assert!(history.body.contains(&format!(r#"action="{service_url}/restart""#)), "{}", history.body);
    assert_eq!(restart.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(restart.body.contains("no container on its server"), "{}", restart.body);
    assert!(restarted.body.contains("Restarted."), "{}", restarted.body);
    assert!(!nothing_live.body.contains("/restart"), "{}", nothing_live.body);

    // Stopping and starting: refused the same way without a container. A stopped service
    // offers Start alone, and is marked stopped here and in its environment's list.
    let stop = web.post(&format!("{service_url}/stop"), Some(&session), &form).await;
    let stopped_notice = web.get(&format!("{service_url}?stopped=1"), Some(&session)).await;
    let stopped_service = control.store().set_service_stopped(web_app.id, true).await.unwrap();
    let stopped = web.get(&service_url, Some(&session)).await;
    let environment = web.get(&format!("/environments/{}", stopped_service.environment_id), Some(&session)).await;
    let start = web.post(&format!("{service_url}/start"), Some(&session), &form).await;

    assert!(history.body.contains(&format!(r#"action="{service_url}/stop""#)), "{}", history.body);
    assert!(!history.body.contains(&format!(r#"action="{service_url}/start""#)), "{}", history.body);
    assert_eq!(stop.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(stop.body.contains("no container on its server"), "{}", stop.body);
    assert!(stopped_notice.body.contains("Stopped. It stays stopped"), "{}", stopped_notice.body);
    assert!(stopped.body.contains(&format!(r#"action="{service_url}/start""#)), "{}", stopped.body);
    for gone in ["/stop\"", "/restart\"", r#"hx-boost="false" class="inline-flex"#] {
        assert!(!stopped.body.contains(gone), "{gone}: {}", stopped.body);
    }
    assert!(stopped.body.contains(">stopped</span>") && stopped.body.contains("Its container is kept"), "{}", stopped.body);
    assert!(environment.body.contains(">stopped</span>"), "{}", environment.body);
    assert_eq!(start.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(start.body.contains("no container on its server"), "{}", start.body);

    // A crash loop marks the service and points at the container's logs.
    control.store().set_service_stopped(web_app.id, false).await.unwrap();
    let new = NewCrashLoop {
        service_id: web_app.id,
        deployment_id: second.id,
        server_id: server.id,
        reason: CrashReason::Restarting,
        restarts: 4,
    };
    control.store().create_crash_loop(new).await.unwrap();
    let crashing = web.get(&service_url, Some(&viewer)).await;
    let listed = web.get(&format!("/environments/{}", production.id), Some(&viewer)).await;
    let project = web.get(&format!("/projects/{}", shop.id), Some(&viewer)).await;

    assert!(crashing.body.contains(">crashing</span>"), "{}", crashing.body);
    assert!(crashing.body.contains("Its container keeps crashing: restarted 4 times since"), "{}", crashing.body);
    assert!(crashing.body.contains(&format!("{service_url}/logs?deployment_id={}", second.id)), "{}", crashing.body);
    assert_eq!(listed.body.matches(">crashing</span>").count(), 1, "{}", listed.body);
    assert_eq!(project.body.matches(">crashing</span>").count(), 1, "{}", project.body);

    // The overview says so first, for whoever may see the service; the server's agent never
    // connected, so there is nothing about it.
    let overview = web.get("/", Some(&viewer)).await;
    let admins = web.get("/", Some(&session)).await;

    assert!(overview.body.contains("Needs attention") && overview.body.contains("Storefront is crashing"), "{}", overview.body);
    assert!(overview.body.contains("Its container was restarted 4 times."), "{}", overview.body);
    assert!(overview.body.contains(&format!("{service_url}/logs?deployment_id={}", second.id)), "{}", overview.body);
    assert!(admins.body.contains("Storefront is crashing") && !admins.body.contains("disconnected"), "{}", admins.body);
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
    assert!(!web.get(&format!("/services/{service_id}"), Some(&viewer)).await.body.contains(&settings));
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
    let previews =
        post(format!("/environments/{environment_id}/settings/previews"), vec![field("previews", "on"), field("revision", "2")])
            .await;
    assert_eq!(previews.header("location"), Some(format!("/environments/{environment_id}/settings?saved=previews").as_str()));
    let page = web.get(&format!("/environments/{environment_id}/settings?saved=previews"), Some(&session)).await.body;
    assert!(page.contains("Deploy a preview of each pull request") && page.contains("Saved."), "{page}");
    let stale = post(format!("/environments/{environment_id}/settings/previews"), vec![field("revision", "2")]).await;
    assert_eq!(stale.status, StatusCode::UNPROCESSABLE_ENTITY);
    let base = control.get_environment(&admin, environment_id.parse().unwrap()).await.unwrap();
    assert!(base.previews);
    let fields = spinikus_core::environment::EnvironmentFields {
        name: "PR #7".to_string(),
        slug: "pr-7".parse().unwrap(),
        previews: false,
    };
    let preview =
        spinikus_store::EnvironmentRepository::create_preview_environment(control.store(), base.id, 7, fields).await.unwrap();
    let preview_page = web.get(&format!("/environments/{}", preview.id), Some(&session)).await.body;
    assert!(preview_page.contains("The preview of pull request #7, copied from Live."), "{preview_page}");
    let preview_settings = web.get(&format!("/environments/{}/settings", preview.id), Some(&session)).await.body;
    assert!(preview_settings.contains("Delete preview") && !preview_settings.contains("Deploy a preview"), "{preview_settings}");
    assert_eq!(post(format!("/environments/{}/delete", preview.id), vec![]).await.status, StatusCode::SEE_OTHER);
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
    let production =
        NewEnvironment { name: "Production".to_string(), slug: None, kind: EnvironmentKind::Production, previews: false };
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
        ("release", "./migrate"),
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
        Some(&serde_json::json!({ "port": 3000, "start": "./new", "release": "./migrate" })),
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
    let production =
        NewEnvironment { name: "Production".to_string(), slug: None, kind: EnvironmentKind::Production, previews: false };
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

#[tokio::test]
async fn instance_admins_manage_storage_destinations() {
    use spinikus_store::StorageDestinationRepository;

    let mut web = Web::new().await;
    web.control = web.control.clone().with_backup_store(std::sync::Arc::new(object_store::memory::InMemory::new()));
    web.router = spinikus_web::router(web.control.clone());
    let (_, _, _, _) = shop_with_team(&web).await;
    let ada = web.sign_in("ada%40example.com").await;
    let dev = web.sign_in("dev%40example.com").await;
    let page = web.get("/settings/storage", Some(&ada)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains(r#"href="/settings/storage""#), "the header links to the settings");
    let token = page.csrf();
    assert_eq!(web.get("/settings/storage", Some(&dev)).await.status, StatusCode::FORBIDDEN);
    assert!(!web.get("/", Some(&dev)).await.body.contains(r#"href="/settings/storage""#));

    let fields = |name: &str, secret: &str| {
        form(&[
            ("_csrf", &token),
            ("name", name),
            ("endpoint", ""),
            ("region", ""),
            ("bucket", "backups"),
            ("path_prefix", "shop"),
            ("access_key_id", "AKIA123"),
            ("secret_access_key", secret),
        ])
    };
    let refused = web.post("/settings/storage", Some(&ada), &fields("", "s3cr3t-key")).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!refused.body.contains("s3cr3t-key"), "the secret is never sent back");
    assert_eq!(web.post("/settings/storage", Some(&ada), &fields("Offsite", "s3cr3t-key")).await.status, StatusCode::SEE_OTHER);
    let admin = web.control.authenticate_session(&ada).await.unwrap();
    let destination = web.control.list_storage_destinations(&admin).await.unwrap().remove(0);
    assert_eq!((destination.fields.region.as_str(), destination.fields.path_prefix.as_str()), ("us-east-1", "shop/"));
    let page = web.get("/settings/storage", Some(&ada)).await;
    assert!(page.body.contains("Offsite") && page.body.contains("Amazon S3") && !page.body.contains("s3cr3t-key"));

    let tested = web.post(&format!("/settings/storage/{}/test", destination.id), Some(&ada), &format!("_csrf={token}")).await;
    assert_eq!(tested.status, StatusCode::OK);
    assert!(tested.body.contains("wrote and deleted a test object"), "{}", tested.body);
    assert_eq!(tested.header("hx-replace-url"), Some("/settings/storage"));

    // Editing keeps the secret when it is left empty.
    let edit = format!("/settings/storage/{}", destination.id);
    let edit_page = web.get(&edit, Some(&ada)).await;
    assert!(edit_page.body.contains("AKIA123") && !edit_page.body.contains("s3cr3t-key"));
    let changes = format!("{}&path_style=on&revision={}", fields("Offsite R2", ""), destination.revision);
    assert_eq!(web.post(&edit, Some(&ada), &changes).await.status, StatusCode::SEE_OTHER);
    let updated = web.control.get_storage_destination(&admin, destination.id).await.unwrap();
    assert_eq!((updated.fields.name.as_str(), updated.fields.path_style), ("Offsite R2", true));
    assert_eq!(web.control.store().storage_destination_secret(destination.id).await.unwrap().expose(), "s3cr3t-key");
    let stale = web.post(&edit, Some(&ada), &changes).await;
    assert!(stale.body.contains("Someone changed this"));

    let deleted = web.post(&format!("{edit}/delete"), Some(&ada), &format!("_csrf={token}")).await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER);
    assert!(web.control.list_storage_destinations(&admin).await.unwrap().is_empty());
}

#[tokio::test]
async fn instance_admins_manage_notification_channels() {
    use spinikus_core::notification::{ChannelSettings, NotificationEvent};
    use spinikus_store::NotificationRepository;

    let web = Web::new().await;
    shop_with_team(&web).await;
    let ada = web.sign_in("ada%40example.com").await;
    let dev = web.sign_in("dev%40example.com").await;
    let page = web.get("/settings/notifications", Some(&ada)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("No notification channels yet.") && page.body.contains(r#"id="channel-kind-email""#));
    let token = page.csrf();
    assert_eq!(web.get("/settings/notifications", Some(&dev)).await.status, StatusCode::FORBIDDEN);

    let secret = "https://hooks.slack.com/services/T0/B0/s3cr3t";
    let slack = |name: &str, secret: &str| {
        form(&[
            ("_csrf", &token),
            ("name", name),
            ("kind", "slack"),
            ("slack_url", secret),
            ("bot_token", "not-this-one"),
            ("events", "deployment.failed"),
            ("events", "service.crashing"),
            ("enabled", "on"),
        ])
    };
    let refused = web.post("/settings/notifications", Some(&ada), &slack("On-call", "")).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(refused.body.contains("needs its webhook URL"), "{}", refused.body);
    assert!(refused.body.contains(r#"value="On-call""#), "what was typed stays");
    let no_events = form(&[("_csrf", &token), ("name", "Quiet"), ("kind", "slack"), ("slack_url", secret)]);
    let refused = web.post("/settings/notifications", Some(&ada), &no_events).await;
    assert!(refused.body.contains("Choose at least one event") && !refused.body.contains("s3cr3t"));
    assert_eq!(web.post("/settings/notifications", Some(&ada), &slack("On-call", secret)).await.status, StatusCode::SEE_OTHER);

    let admin = web.control.authenticate_session(&ada).await.unwrap();
    let channel = web.control.list_notification_channels(&admin).await.unwrap().remove(0);
    assert_eq!(channel.fields.settings, ChannelSettings::Slack {});
    assert_eq!(channel.fields.events, [NotificationEvent::DeploymentFailed, NotificationEvent::ServiceCrashing]);
    assert_eq!(web.control.store().notification_channel_secret(channel.id).await.unwrap().unwrap().expose(), secret);
    let page = web.get("/settings/notifications", Some(&ada)).await;
    assert!(page.body.contains("On-call") && page.body.contains("2 events") && !page.body.contains("s3cr3t"));

    // Editing keeps the secret when it is left empty; every event checked is every event.
    let edit = format!("/settings/notifications/{}", channel.id);
    let edit_page = web.get(&edit, Some(&ada)).await;
    assert!(edit_page.body.contains("Nothing sent to this channel yet.") && !edit_page.body.contains("s3cr3t"));
    assert!(edit_page.body.contains(r#"id="channel-kind-slack" type="radio" name="kind" value="slack" checked"#));
    let mut every = vec![("_csrf", token.as_str()), ("name", "Team"), ("kind", "slack"), ("slack_url", "")];
    every.extend(NotificationEvent::ALL.iter().map(|event| ("events", event.as_str())));
    let revision = channel.revision.to_string();
    every.push(("revision", &revision));
    assert_eq!(web.post(&edit, Some(&ada), &form(&every)).await.status, StatusCode::SEE_OTHER);
    let updated = web.control.get_notification_channel(&admin, channel.id).await.unwrap();
    assert_eq!((updated.fields.name.as_str(), updated.fields.events.is_empty(), updated.fields.enabled), ("Team", true, false));
    assert!(updated.has_secret);
    let stale = web.post(&edit, Some(&ada), &form(&every)).await;
    assert!(stale.body.contains("Someone changed this"));

    // A webhook nothing listens on: the test says it wasn't delivered.
    let revision = updated.revision.to_string();
    let webhook = form(&[
        ("_csrf", &token),
        ("name", "Hook"),
        ("kind", "webhook"),
        ("url", "http://127.0.0.1:9/hook"),
        ("events", "backup.failed"),
        ("enabled", "on"),
        ("revision", &revision),
    ]);
    assert_eq!(web.post(&edit, Some(&ada), &webhook).await.status, StatusCode::SEE_OTHER);
    let updated = web.control.get_notification_channel(&admin, channel.id).await.unwrap();
    assert!(!updated.has_secret, "a new kind drops the old secret");
    let tested = web.post(&format!("{edit}/test"), Some(&ada), &format!("_csrf={token}")).await;
    assert_eq!(tested.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(tested.body.contains("Hook: The test was not delivered"), "{}", tested.body);
    assert_eq!(tested.header("hx-replace-url"), Some("/settings/notifications"));

    let deleted = web.post(&format!("{edit}/delete"), Some(&ada), &format!("_csrf={token}")).await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER);
    assert!(web.control.list_notification_channels(&admin).await.unwrap().is_empty());
}

#[tokio::test]
async fn shows_a_services_and_a_servers_resources() {
    use spinikus_core::{
        id::DeploymentId,
        metrics::{MetricPoint, period_end},
        server::{InstanceHealth, InstanceStatus, ObservedInstance, ServerFields},
    };
    use spinikus_store::{InstanceRepository, ServerRepository};

    let web = Web::new().await;
    let (_, _, _, service) = shop_with_team(&web).await;
    let control = &web.control;
    let server = control.store().get_or_create_embedded_server(ServerFields { name: "this machine".to_string() }).await.unwrap();
    let container = ObservedInstance {
        service_id: service.id,
        deployment_id: DeploymentId::new(),
        container_id: "web-1".to_string(),
        status: InstanceStatus::Running,
        health: InstanceHealth::None,
        restart_count: 0,
        started_at: None,
    };
    control.store().upsert_instance(server.id, container).await.unwrap();
    let minute = jiff::SignedDuration::from_mins(1);
    let now = period_end(jiff::Timestamp::now(), minute) - minute;
    let point = |minutes_ago: i32, cores: f64| MetricPoint {
        at: now - minute * minutes_ago,
        cpu_cores: Some(cores),
        memory_bytes: Some(64 * 1024 * 1024),
        network_rx_rate: Some(1536.0),
        network_tx_rate: Some(512.0),
        ..MetricPoint::default()
    };
    let disk = MetricPoint {
        disk_used_bytes: Some(95),
        disk_total_bytes: Some(100),
        memory_limit_bytes: Some(1 << 30),
        ..point(0, 2.0)
    };
    let points = vec![("web-1".to_string(), point(2, 0.25)), ("web-1".to_string(), point(1, 0.5)), (String::new(), disk)];
    control.record_stats(server.id, points).await.unwrap();
    let ada = web.sign_in("ada%40example.com").await;
    let viewer = web.sign_in("viewer%40example.com").await;

    let page = web.get(&format!("/services/{}/metrics", service.id), Some(&viewer)).await;
    assert_eq!(page.status, StatusCode::OK);
    for expected in ["CPU (cores)", "Memory", "Network", ">0.50<", "1.5 KiB/s", "Received", "Sent", "<svg", "<title>", "Table"] {
        assert!(page.body.contains(expected), "{expected} in {}", page.body);
    }
    assert!(page.body.contains(r#"aria-current="true""#) && page.body.contains("?range=7d"));
    assert!(!page.body.contains("Disk"), "a service has no disk chart");
    let week = web.get(&format!("/services/{}/metrics?range=7d", service.id), Some(&viewer)).await;
    assert!(week.body.contains("Nothing measured in this range"), "hours are rolled up later");
    let tab = web.get(&format!("/services/{}", service.id), Some(&viewer)).await;
    assert!(tab.body.contains(&format!(r#"href="/services/{}/metrics""#, service.id)));

    let server_page = web.get(&format!("/settings/servers/{}?range=6h", server.id), Some(&ada)).await;
    assert_eq!(server_page.status, StatusCode::OK);
    for expected in ["Resources", "Disk", "of 1.0 GiB", "of 100 B", "over 90% full"] {
        assert!(server_page.body.contains(expected), "{expected} in {}", server_page.body);
    }
}

#[tokio::test]
async fn creates_databases_and_backs_them_up_and_restores_them() {
    use spinikus_control::NewStorageDestination;
    use spinikus_core::{backup::BackupStatus, server::ServerFields};
    use spinikus_store::{BackupRepository, ServerRepository};

    let web = Web::new().await;
    let control = &web.control;
    let (admin, _, production, _) = shop_with_team(&web).await;
    control.store().get_or_create_embedded_server(ServerFields { name: "this machine".to_string() }).await.unwrap();
    let ada = web.sign_in("ada%40example.com").await;
    let dev = web.sign_in("dev%40example.com").await;
    let viewer = web.sign_in("viewer%40example.com").await;
    let environment_page = web.get(&format!("/environments/{}", production.id), Some(&dev)).await;
    assert!(environment_page.body.contains("New database"));
    let token = environment_page.csrf();

    // A database, from the newest PostgreSQL by default or a chosen version.
    let new_page = web.get(&format!("/environments/{}/databases/new", production.id), Some(&dev)).await;
    assert!(new_page.body.contains(r#"value="postgres:18" selected"#) && new_page.body.contains("Redis 7"), "{}", new_page.body);
    assert!(new_page.body.contains(r#"name="deploy" type="checkbox" checked"#), "deployed by default: {}", new_page.body);
    let databases = format!("/environments/{}/databases", production.id);
    let unknown = web.post(&databases, Some(&dev), &form(&[("_csrf", &token), ("name", "Main"), ("database", "mysql:8")])).await;
    assert_eq!(unknown.status, StatusCode::UNPROCESSABLE_ENTITY);
    let main = form(&[("_csrf", &token), ("name", "Main DB"), ("database", "postgres:17"), ("deploy", "on")]);
    let reply = web.post(&databases, Some(&dev), &main).await;
    let service_id = created(&reply, "/services/");
    assert_eq!(control.list_deployments(&admin, service_id.parse().unwrap(), None, 10).await.unwrap().items.len(), 1);
    // Unchecked, it waits on its page to be deployed.
    let later = web.post(&databases, Some(&dev), &form(&[("_csrf", &token), ("name", "Later"), ("database", "redis:8")])).await;
    let later = created(&later, "/services/");
    let (later_id, query) = later.split_once('?').unwrap();
    assert_eq!(query, "created=undeployed");
    assert!(control.list_deployments(&admin, later_id.parse().unwrap(), None, 10).await.unwrap().items.is_empty());
    let later_page = web.get(&format!("/services/{later}"), Some(&dev)).await;
    assert!(later_page.body.contains("Created, not deployed yet."), "{}", later_page.body);
    let id: spinikus_core::id::ServiceId = service_id.parse().unwrap();
    let source = control.get_source(&admin, id).await.unwrap();
    assert_eq!(source.spec, SourceSpec::Image(ImageSource { image: "postgres:17".to_string() }));
    let service_page = web.get(&format!("/services/{service_id}"), Some(&viewer)).await;
    assert!(
        service_page.body.contains("${{ main-db.DATABASE_URL }}") && service_page.body.contains("main-db:5432"),
        "{}",
        service_page.body
    );
    assert!(
        service_page.body.contains(&format!(r#"href="/services/{service_id}/backups""#))
            && !service_page.body.contains("/domains")
    );

    // Without a storage destination, the page says where to add one.
    let backups = format!("/services/{service_id}/backups");
    assert!(web.get(&backups, Some(&ada)).await.body.contains(r#"href="/settings/storage""#));
    let new = NewStorageDestination {
        name: "Offsite".to_string(),
        endpoint: None,
        region: None,
        bucket: "backups".to_string(),
        path_prefix: None,
        path_style: false,
        access_key_id: "AKIA".to_string(),
        secret_access_key: Secret::new("key"),
    };
    let destination = control.create_storage_destination(&admin, new).await.unwrap().id.to_string();

    // Backing up now: developers may; the list refreshes itself while it is queued.
    let dev_page = web.get(&backups, Some(&dev)).await;
    assert!(dev_page.body.contains("Back up now") && !dev_page.body.contains("Add a schedule"));
    assert_eq!(
        web.post(&backups, Some(&dev), &form(&[("_csrf", &token), ("destination_id", &destination)])).await.status,
        StatusCode::SEE_OTHER
    );
    let page = web.get(&backups, Some(&ada)).await;
    assert!(page.body.contains("queued") && page.body.contains(r#"hx-trigger="every 2s""#), "{}", page.body);
    let activity = web.get(&format!("{backups}/activity"), Some(&viewer)).await;
    assert!(activity.body.contains("by hand") && !activity.body.contains("<html"));
    let viewer_page = web.get(&backups, Some(&viewer)).await;
    assert!(!viewer_page.body.contains("Back up now") && !viewer_page.body.contains("Restore"));

    // Schedules: admins add, turn off and delete them.
    let admin_token = page.csrf();
    let schedules = format!("/services/{service_id}/backup-schedules");
    let schedule =
        |cron: &str| form(&[("_csrf", &admin_token), ("destination_id", &destination), ("cron", cron), ("keep_last", "3")]);
    let refused = web.post(&schedules, Some(&ada), &schedule("every day")).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(refused.body.contains("every day"), "what was typed stays: {}", refused.body);
    assert_eq!(
        web.post(&schedules, Some(&dev), &schedule("0 3 * * *").replace(&admin_token, &token)).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(web.post(&schedules, Some(&ada), &schedule("0 3 * * *")).await.status, StatusCode::SEE_OTHER);
    let made = control.list_backup_schedules(&admin, id).await.unwrap().remove(0);
    assert!(web.get(&backups, Some(&ada)).await.body.contains("keeps 3"));
    let off = form(&[("_csrf", &admin_token), ("enabled", "false"), ("revision", &made.revision.to_string())]);
    assert_eq!(web.post(&format!("/backup-schedules/{}", made.id), Some(&ada), &off).await.status, StatusCode::SEE_OTHER);
    assert_eq!(control.get_backup_schedule(&admin, made.id).await.unwrap().next_run_at, None);
    assert!(web.get(&backups, Some(&ada)).await.body.contains("Turn on"));
    let deleted = web.post(&format!("/backup-schedules/{}/delete", made.id), Some(&ada), &format!("_csrf={admin_token}")).await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER);

    // Once it succeeded, an admin can restore it, into a database that runs.
    let backup = control.list_backups(&admin, id, None, 10).await.unwrap().items.remove(0);
    let server = control.store().find_embedded_server().await.unwrap().unwrap();
    control.store().start_backup(backup.id, server.id).await.unwrap();
    control.store().finish_backup(backup.id, Ok(3 * 1024 * 1024)).await.unwrap();
    let page = web.get(&backups, Some(&ada)).await;
    assert!(page.body.contains("succeeded") && page.body.contains("3.0 MB") && !page.body.contains("every 2s"), "{}", page.body);
    let restore = format!("/backups/{}/restore", backup.id);
    let not_live = web.post(&restore, Some(&ada), &format!("_csrf={admin_token}")).await;
    assert_eq!(not_live.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(not_live.body.contains("nothing live"), "{}", not_live.body);
    let deployment = control.list_deployments(&admin, id, None, 1).await.unwrap().items.remove(0);
    go_live(control, deployment.id, "postgres@sha256:17").await;
    let restored = web.post(&restore, Some(&ada), &format!("_csrf={admin_token}")).await;
    assert_eq!(restored.header("location"), Some(format!("{backups}?restoring=1").as_str()));
    let page = web.get(&format!("{backups}?restoring=1"), Some(&ada)).await;
    assert!(page.body.contains("Restore queued") && page.body.contains("waiting for the safety backup"), "{}", page.body);
    assert_eq!(web.post(&restore, Some(&dev), &format!("_csrf={token}")).await.status, StatusCode::FORBIDDEN);

    // Or into a new database, which waits to run before the restore.
    let environment = control.get_service(&admin, id).await.unwrap().environment_id;
    let new_page = format!("/environments/{environment}/databases/new?from_backup={}", backup.id);
    assert!(page.body.contains(&new_page.replace('&', "&amp;")) || page.body.contains(&new_page), "{}", page.body);
    let page = web.get(&new_page, Some(&ada)).await;
    assert!(page.body.contains("Starts with the data of"), "{}", page.body);
    assert!(page.body.contains(r#"value="postgres:"#) && !page.body.contains(r#"value="redis:"#), "{}", page.body);
    let fields = form(&[
        ("_csrf", &page.csrf()),
        ("name", "Copy"),
        ("slug", ""),
        ("database", "postgres:18"),
        ("server_id", ""),
        ("from_backup", &backup.id.to_string()),
    ]);
    let created = web.post(&format!("/environments/{environment}/databases"), Some(&ada), &fields).await;
    assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.body);
    let copy = created.header("location").unwrap().to_string();
    assert!(copy.ends_with("/backups"), "{copy}");
    assert!(web.get(&copy, Some(&ada)).await.body.contains("waiting for the database to start"));

    // A failed backup can be deleted; one still queued can't.
    let backups_now = control.list_backups(&admin, id, None, 10).await.unwrap().items;
    let safety = backups_now.iter().find(|item| item.status == BackupStatus::Queued).unwrap();
    let refused = web.post(&format!("/backups/{}/delete", safety.id), Some(&ada), &format!("_csrf={admin_token}")).await;
    assert!(refused.body.contains("has not finished yet"), "{}", refused.body);
    control.store().finish_backup(safety.id, Err("pg_dump: connection refused".to_string())).await.unwrap();
    assert!(web.get(&backups, Some(&ada)).await.body.contains("pg_dump: connection refused"));
    let deleted = web.post(&format!("/backups/{}/delete", safety.id), Some(&ada), &format!("_csrf={admin_token}")).await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn backs_up_an_apps_volumes_and_restores_them() {
    use spinikus_control::{NewStorageDestination, NewVolume};
    use spinikus_core::server::ServerFields;
    use spinikus_store::{BackupRepository, ServerRepository};

    let web = Web::new().await;
    let control = &web.control;
    let (admin, _, _, service) = shop_with_team(&web).await;
    let server = control.store().get_or_create_embedded_server(ServerFields { name: "this machine".to_string() }).await.unwrap();
    let ada = web.sign_in("ada%40example.com").await;
    let dev = web.sign_in("dev%40example.com").await;
    let backups = format!("/services/{}/backups", service.id);

    // An app has the tab too; without volumes, the page says what to add.
    let service_page = web.get(&format!("/services/{}", service.id), Some(&dev)).await;
    assert!(service_page.body.contains(&format!(r#"href="{backups}""#)), "{}", service_page.body);
    let page = web.get(&backups, Some(&dev)).await;
    assert!(page.body.contains("it has none") && page.body.contains("/settings#volumes"), "{}", page.body);
    let new = NewStorageDestination {
        name: "Offsite".to_string(),
        endpoint: None,
        region: None,
        bucket: "backups".to_string(),
        path_prefix: None,
        path_style: false,
        access_key_id: "AKIA".to_string(),
        secret_access_key: Secret::new("key"),
    };
    let destination = control.create_storage_destination(&admin, new).await.unwrap().id.to_string();
    let token = page.csrf();
    let refused = web.post(&backups, Some(&dev), &form(&[("_csrf", &token), ("destination_id", &destination)])).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(refused.body.contains("no volumes"), "{}", refused.body);

    // With two volumes, the form asks which one.
    let volume = |name: &str, path: &str| NewVolume { name: name.to_string(), mount_path: path.to_string() };
    let uploads = control.create_volume(&admin, service.id, volume("uploads", "/app/uploads")).await.unwrap();
    control.create_volume(&admin, service.id, volume("cache", "/app/cache")).await.unwrap();
    let page = web.get(&backups, Some(&dev)).await;
    assert!(page.body.contains(r#"name="volume_id""#) && page.body.contains("uploads (/app/uploads)"), "{}", page.body);
    let unnamed = web.post(&backups, Some(&dev), &form(&[("_csrf", &token), ("destination_id", &destination)])).await;
    assert_eq!(unnamed.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(unnamed.body.contains("name the one to back up"), "{}", unnamed.body);
    let fields = form(&[("_csrf", &token), ("destination_id", &destination), ("volume_id", &uploads.id.to_string())]);
    assert_eq!(web.post(&backups, Some(&dev), &fields).await.status, StatusCode::SEE_OTHER);

    // Its row names the volume; restoring is into it, never into a new database.
    let backup = control.list_backups(&admin, service.id, None, 10).await.unwrap().items.remove(0);
    assert_eq!(backup.volume_id, Some(uploads.id));
    control.store().start_backup(backup.id, server.id).await.unwrap();
    control.store().finish_backup(backup.id, Ok(2048)).await.unwrap();
    let page = web.get(&backups, Some(&ada)).await;
    assert!(page.body.contains("uploads") && page.body.contains("Restore this backup into uploads?"), "{}", page.body);
    assert!(!page.body.contains("Restore into a new database"), "{}", page.body);
    let restored = web.post(&format!("/backups/{}/restore", backup.id), Some(&ada), &format!("_csrf={}", page.csrf())).await;
    assert_eq!(restored.header("location"), Some(format!("{backups}?restoring=1").as_str()), "{}", restored.body);
    let safety = control.list_backups(&admin, service.id, None, 10).await.unwrap().items.remove(0);
    assert_eq!(safety.volume_id, Some(uploads.id), "the safety backup is of the same volume");
}

#[tokio::test]
async fn instance_admins_change_the_general_settings() {
    let web = Web::new().await;
    let (admin, _, _, _) = shop_with_team(&web).await;
    let ada = web.sign_in("ada%40example.com").await;
    let dev = web.sign_in("dev%40example.com").await;
    assert_eq!(web.get("/settings", Some(&ada)).await.header("location"), Some("/settings/general"));
    assert_eq!(web.get("/settings", Some(&dev)).await.header("location"), Some("/settings/account"));
    assert_eq!(web.get("/settings/general", Some(&dev)).await.status, StatusCode::FORBIDDEN);
    let page = web.get("/settings/general", Some(&ada)).await;
    assert!(page.body.contains(r#"value="localhost""#), "the default base domain is shown");
    let token = page.csrf();

    let domains =
        |domain: &str, revision: &str| form(&[("_csrf", &token), ("base_domain", domain), ("base_domain_revision", revision)]);
    let saved = web.post("/settings/general/domains", Some(&ada), &domains("apps.example.com", "0")).await;
    let stale = web.post("/settings/general/domains", Some(&ada), &domains("other.example.com", "0")).await;
    let https = form(&[
        ("_csrf", &token),
        ("acme_email", "not an email"),
        ("acme_email_revision", "0"),
        ("acme_directory_url", ""),
        ("acme_directory_url_revision", "0"),
    ]);
    let bad_email = web.post("/settings/general/https", Some(&ada), &https).await;
    let releases = |count: &str| form(&[("_csrf", &token), ("releases_kept", count), ("releases_kept_revision", "0")]);
    let bad_count = web.post("/settings/general/releases", Some(&ada), &releases("many")).await;
    let count = web.post("/settings/general/releases", Some(&ada), &releases("3")).await;
    let by_dev = web.post("/settings/general/domains", Some(&dev), &domains("x.example.com", "1")).await;
    let dashboard = |domain: &str, revision: &str| {
        form(&[("_csrf", &token), ("dashboard_domain", domain), ("dashboard_domain_revision", revision)])
    };
    let wildcard = web.post("/settings/general/dashboard", Some(&ada), &dashboard("*.example.com", "0")).await;
    let dashboard_saved = web.post("/settings/general/dashboard", Some(&ada), &dashboard("spinikus.example.com", "0")).await;

    assert_eq!(saved.header("location"), Some("/settings/general?saved=domains"));
    assert_eq!(stale.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(stale.body.contains("Someone changed this") && stale.body.contains("other.example.com"), "what was typed is kept");
    assert_eq!(bad_email.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(bad_email.body.contains("Give an email address"), "{}", bad_email.body);
    assert_eq!(bad_count.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(count.status, StatusCode::SEE_OTHER);
    assert_eq!(by_dev.status, StatusCode::FORBIDDEN);
    assert_eq!(wildcard.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(wildcard.body.contains("not a wildcard") && wildcard.body.contains("*.example.com"), "{}", wildcard.body);
    assert_eq!(dashboard_saved.header("location"), Some("/settings/general?saved=dashboard"));
    let setting = |key| web.control.get_setting(&admin, key);
    assert_eq!(setting("dashboard_domain").await.unwrap().value, "spinikus.example.com");
    assert_eq!(setting("base_domain").await.unwrap().value, "apps.example.com");
    assert_eq!(setting("releases_kept").await.unwrap().value, 3);
    // The directory was saved before the email was refused; the default it was given is no change.
    assert_eq!(setting("acme_email").await.unwrap().value, serde_json::Value::Null);

    // Empty turns the generated hostnames off.
    let page = web.get("/settings/general?saved=domains", Some(&ada)).await;
    assert!(page.body.contains("Saved."));
    let revision = setting("base_domain").await.unwrap().revision.to_string();
    assert_eq!(web.post("/settings/general/domains", Some(&ada), &domains("", &revision)).await.status, StatusCode::SEE_OTHER);
    assert_eq!(setting("base_domain").await.unwrap().value, serde_json::Value::Null);
}

#[tokio::test]
async fn admins_add_servers_and_developers_place_services_on_them() {
    let web = Web::new().await;
    let (admin, _, production, service) = shop_with_team(&web).await;
    let ada = web.sign_in("ada%40example.com").await;
    let dev = web.sign_in("dev%40example.com").await;
    let token = web.get("/", Some(&ada)).await.csrf();
    let dev_token = web.get("/", Some(&dev)).await.csrf();
    let settings = format!("/services/{}/settings", service.id);

    // With only the control plane's server there is nothing to choose.
    web.control.embedded_server("local").await.unwrap();
    assert!(!web.get(&settings, Some(&dev)).await.body.contains("Placement"));
    assert_eq!(web.get("/settings/servers", Some(&dev)).await.status, StatusCode::FORBIDDEN);
    let refused = web.post("/settings/servers", Some(&ada), &form(&[("_csrf", &token), ("name", " ")])).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);

    let added = web.post("/settings/servers", Some(&ada), &form(&[("_csrf", &token), ("name", "web-2")])).await;
    let id = created(&added, "/settings/servers/");
    let list = web.get("/settings/servers", Some(&ada)).await;
    assert!(list.body.contains("web-2") && list.body.contains("waiting for its agent"), "{}", list.body);
    assert!(list.body.contains("(this control plane)"));
    let join = web.post(&format!("/settings/servers/{id}/join-token"), Some(&ada), &form(&[("_csrf", &token)])).await;
    assert_eq!(join.status, StatusCode::OK);
    assert_eq!(join.header("hx-replace-url"), Some(format!("/settings/servers/{id}").as_str()));
    assert!(join.body.contains("SPINIKUS_JOIN_TOKEN=spj_"), "{}", join.body);
    assert!(join.body.contains("spinikus agent join https://spinikus.test:8443 --ca-fingerprint sha256:"));
    let install = concat!(
        "releases/download/v",
        env!("CARGO_PKG_VERSION"),
        "/install.sh | sudo sh -s -- --join https://spinikus.test:8443 --token spj_"
    );
    assert!(join.body.contains(install), "{}", join.body);
    assert!(!web.get(&format!("/settings/servers/{id}"), Some(&ada)).await.body.contains("spj_"), "shown only once");

    // Developers see the server among the choices, and place the service there.
    let page = web.get(&settings, Some(&dev)).await;
    assert!(
        page.body.contains("Placement")
            && page.body.contains("web-2")
            && page.body.contains("local (the control plane's server)"),
        "{}",
        page.body
    );
    let placement =
        |server: &str| form(&[("_csrf", &dev_token), ("server_id", server), ("revision", &service.revision.to_string())]);
    let placed = web.post(&format!("{settings}/placement"), Some(&dev), &placement(&id)).await;
    assert_eq!(placed.header("location"), Some(format!("{settings}?saved=placement").as_str()));
    let placed_service = web.control.get_service(&admin, service.id).await.unwrap();
    assert_eq!(placed_service.server_id.map(|id| id.to_string()), Some(id.clone()));
    let bogus = web.post(&format!("{settings}/placement"), Some(&dev), &placement("srv_bogus")).await;
    assert_eq!(bogus.status, StatusCode::UNPROCESSABLE_ENTITY);

    // New databases and services can go there too.
    let new_database = web.get(&format!("/environments/{}/databases/new", production.id), Some(&dev)).await;
    assert!(new_database.body.contains("web-2"));
    let database = form(&[
        ("_csrf", &dev_token),
        ("name", "Cache"),
        ("slug", ""),
        ("database", "redis:8"),
        ("server_id", &id),
        ("deploy", "on"),
    ]);
    let reply = web.post(&format!("/environments/{}/databases", production.id), Some(&dev), &database).await;
    let cache = web.control.get_service(&admin, created(&reply, "/services/").parse().unwrap()).await.unwrap();
    assert_eq!(cache.server_id.map(|id| id.to_string()), Some(id.clone()));

    // A revoked server can't be chosen any more, but the service still names it.
    let revoked = web.post(&format!("/settings/servers/{id}/revoke"), Some(&ada), &form(&[("_csrf", &token)])).await;
    assert_eq!(revoked.status, StatusCode::SEE_OTHER);
    let page = web.get(&format!("/settings/servers/{id}"), Some(&ada)).await;
    assert!(page.body.contains("revoked") && !page.body.contains("Create a join token"));
    assert!(!web.get("/settings/servers", Some(&ada)).await.body.contains(&format!("/settings/servers/{id}")));
    assert!(web.get(&settings, Some(&dev)).await.body.contains("web-2 (revoked)"));
    let new_service = web.get(&format!("/environments/{}/services/new", production.id), Some(&dev)).await;
    assert!(!new_service.body.contains("web-2"));
}

#[tokio::test]
async fn everyone_manages_their_own_api_tokens() {
    let web = Web::new().await;
    let (_, shop, production, _) = shop_with_team(&web).await;
    let dev = web.sign_in("dev%40example.com").await;
    let page = web.get("/settings/tokens", Some(&dev)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("No API tokens.") && page.body.contains("Shop / Production"), "{}", page.body);
    assert!(!page.body.contains(r#"href="/settings/general""#), "only instance admins get the instance's tabs");
    let token = page.csrf();

    let new = |name: &str, scope: &str| {
        form(&[("_csrf", &token), ("name", name), ("role", "developer"), ("scope", scope), ("expires", "30")])
    };
    let refused = web.post("/settings/tokens", Some(&dev), &new("", "")).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    let reply = web.post("/settings/tokens", Some(&dev), &new("CI", &format!("environment:{}", production.id))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("hx-replace-url"), Some("/settings/tokens"));
    let start = reply.body.find("spk_").expect("the secret is shown");
    let secret: String =
        reply.body[start..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
    let principal = web.control.authenticate_api_token(&secret).await.unwrap();
    let tokens = web.control.list_api_tokens(&principal).await.unwrap();
    assert_eq!(tokens.len(), 1);
    assert_eq!((tokens[0].role, tokens[0].scope), (Role::Developer, Some(MembershipScope::Environment(production.id))));
    assert!(tokens[0].expires_at.is_some());
    let page = web.get("/settings/tokens", Some(&dev)).await;
    assert!(page.body.contains("CI") && page.body.contains("Shop / Production") && !page.body.contains(&secret));
    // A project the developer has only an environment in is still theirs to scope to; one
    // they can't see is not a page.
    let in_project = web.post("/settings/tokens", Some(&dev), &new("Shop", &format!("project:{}", shop.id))).await;
    assert_eq!(in_project.status, StatusCode::OK);
    let bad_scope = web.post("/settings/tokens", Some(&dev), &new("Bad", "project:nope")).await;
    assert_eq!(bad_scope.status, StatusCode::UNPROCESSABLE_ENTITY);

    let revoked = web.post(&format!("/settings/tokens/{}/revoke", tokens[0].id), Some(&dev), &form(&[("_csrf", &token)])).await;
    assert_eq!(revoked.header("location"), Some("/settings/tokens"));
    assert!(web.control.authenticate_api_token(&secret).await.is_err());
    // Someone else's token is not theirs to revoke, unless they are an instance admin.
    let viewer = web.sign_in("viewer%40example.com").await;
    let viewer_token = web.get("/", Some(&viewer)).await.csrf();
    let other = web.control.list_api_tokens(&principal).await.unwrap().remove(0);
    web.post(&format!("/settings/tokens/{}/revoke", other.id), Some(&viewer), &form(&[("_csrf", &viewer_token)])).await;
    assert!(web.control.list_api_tokens(&principal).await.unwrap().iter().any(|token| token.id == other.id));
    let ada = web.sign_in("ada%40example.com").await;
    let ada_token = web.get("/", Some(&ada)).await.csrf();
    web.post(&format!("/settings/tokens/{}/revoke", other.id), Some(&ada), &form(&[("_csrf", &ada_token)])).await;
    assert!(web.control.list_api_tokens(&principal).await.unwrap().is_empty());
}

#[tokio::test]
async fn shows_runtime_logs_and_downloads_deployment_logs() {
    let web = Web::new().await;
    let (admin, _, _, service) = shop_with_team(&web).await;
    let viewer = web.sign_in("viewer%40example.com").await;

    // Viewers read the logs; the page follows the stream with what was asked for.
    let page = web.get(&format!("/services/{}/logs?q=time+out&stream=stderr&tail=1000", service.id), Some(&viewer)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains(&format!(r#"href="/services/{}/logs""#, service.id)), "the Logs tab");
    let stream = format!("/services/{}/logs/stream?q=time%20out&amp;stream=stderr&amp;tail=1000", service.id);
    assert!(page.body.contains(&format!(r#"sse-connect="{stream}""#)), "{}", page.body);
    assert!(page.body.contains(r#"<option value="1000" selected>"#));
    assert_eq!(
        web.get(&format!("/services/{}/logs?deployment_id=nope", service.id), Some(&viewer)).await.status,
        StatusCode::NOT_FOUND
    );

    // With nothing live, the stream says so in the log box and ends, rather than failing.
    let events = web.events(&format!("/services/{}/logs/stream", service.id), &viewer, "event: end").await;
    assert!(events.contains("event: log") && events.contains("The service has nothing live"), "{events}");
    let download = web.get(&format!("/services/{}/logs/download", service.id), Some(&viewer)).await;
    assert_eq!(download.header("content-type"), Some("text/plain; charset=utf-8"));
    assert!(download.header("content-disposition").unwrap().starts_with("attachment; filename=\"web-"));
    assert!(download.body.starts_with("# The service has nothing live"), "{}", download.body);

    // A deployment's own log downloads as it is.
    let server = web.control.create_server(&admin, NewServer { name: "web-1".to_string() }).await.unwrap();
    let changes = spinikus_control::ServiceChanges { server_id: Some(Some(server.id)), ..Default::default() };
    web.control.update_service(&admin, service.id, service.revision, changes).await.unwrap();
    let image = SourceSpec::Image(ImageSource { image: "nginx:1".to_string() });
    web.control.create_source(&admin, service.id, image).await.unwrap();
    let deployment = web.control.create_deployment(&admin, service.id).await.unwrap();
    let logs = web._dir.path().join("logs").join("deployments");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(logs.join(format!("{}.log", deployment.id)), "2026-11-01T10:00:00Z ==> preparing\n").unwrap();
    let log = web.get(&format!("/deployments/{}/log", deployment.id), Some(&viewer)).await;
    assert_eq!(log.body, "2026-11-01T10:00:00Z ==> preparing\n");
    assert_eq!(log.header("content-disposition"), Some(format!("attachment; filename=\"{}.log\"", deployment.id).as_str()));
}

#[tokio::test]
async fn admins_open_terminals_from_the_service_and_its_deployments() {
    let web = Web::new().await;
    let (admin_principal, _, _, service) = shop_with_team(&web).await;
    let admin = web.sign_in("ada%40example.com").await;
    let developer = web.sign_in("dev%40example.com").await;
    let page = format!("/services/{}/terminal", service.id);

    // Developers neither see it nor open it.
    let by_developer = web.get(&page, Some(&developer)).await;
    let developers_service = web.get(&format!("/services/{}", service.id), Some(&developer)).await;
    assert_eq!(by_developer.status, StatusCode::FORBIDDEN);
    assert!(!developers_service.body.contains(&page), "{}", developers_service.body);

    // Admins get the Terminal tab, which loads the page in full, with its own CSP.
    let terminal = web.get(&page, Some(&admin)).await;
    let nothing_live = web.get(&format!("/services/{}", service.id), Some(&admin)).await;
    assert_eq!(terminal.status, StatusCode::OK);
    let csp = terminal.header("content-security-policy").unwrap();
    assert!(csp.contains("style-src 'self' 'unsafe-inline'") && csp.contains("script-src 'self';"), "{csp}");
    assert!(!nothing_live.header("content-security-policy").unwrap().contains("unsafe-inline"));
    assert!(terminal.body.contains(&format!(r#"data-socket="{page}/socket""#)), "{}", terminal.body);
    assert!(terminal.body.contains(r#"<body class="#) && terminal.body.contains(r#"hx-boost="false""#));
    assert!(terminal.body.contains("/assets/xterm.min.js?v=") && terminal.body.contains("/assets/terminal.js?v="));
    assert!(nothing_live.body.contains(&format!(r#"href="{page}" hx-boost="false""#)), "the tab: {}", nothing_live.body);
    let button = format!(r#"href="{page}" hx-boost="false" class="inline-flex"#);
    assert!(!nothing_live.body.contains(&button), "no button with nothing live");
    assert_eq!(web.get("/assets/xterm.css", None).await.header("content-type"), Some("text/css; charset=utf-8"));

    // With something live: a button on the service page, and a link on the deployment's page.
    let server = web.control.create_server(&admin_principal, NewServer { name: "web-1".to_string() }).await.unwrap();
    let changes = spinikus_control::ServiceChanges { server_id: Some(Some(server.id)), ..Default::default() };
    web.control.update_service(&admin_principal, service.id, service.revision, changes).await.unwrap();
    let image = SourceSpec::Image(ImageSource { image: "nginx:1".to_string() });
    web.control.create_source(&admin_principal, service.id, image).await.unwrap();
    let deployment = web.control.create_deployment(&admin_principal, service.id).await.unwrap();
    go_live(&web.control, deployment.id, "nginx@sha256:01").await;
    let live = web.get(&format!("/services/{}", service.id), Some(&admin)).await;
    let deployment_page = web.get(&format!("/deployments/{}", deployment.id), Some(&admin)).await;
    let developers_deployment = web.get(&format!("/deployments/{}", deployment.id), Some(&developer)).await;
    assert!(live.body.contains(&button), "{}", live.body);
    let link = format!(r#"href="{page}?deployment_id={}" hx-boost="false""#, deployment.id);
    assert!(deployment_page.body.contains(&link), "{}", deployment_page.body);
    assert!(!developers_deployment.body.contains(&page));
}

#[tokio::test]
async fn the_palette_lists_what_the_user_can_open() {
    let web = Web::new().await;
    let (admin, shop, production, service) = shop_with_team(&web).await;
    let blog = web.control.create_project(&admin, NewProject { name: "Blog".to_string(), slug: None }).await.unwrap();
    let ada = web.sign_in("ada%40example.com").await;
    let dev = web.sign_in("dev%40example.com").await;

    let page = web.get("/", Some(&dev)).await;
    assert!(page.body.contains(r#"<dialog id="palette""#) && page.body.contains("/assets/palette.js?v="));
    let script = web.get("/assets/palette.js", None).await;
    assert_eq!((script.status, script.header("content-type")), (StatusCode::OK, Some("text/javascript; charset=utf-8")));

    let for_dev = web.get("/palette", Some(&dev)).await.body;
    for href in
        [format!("/projects/{}", shop.id), format!("/environments/{}", production.id), format!("/services/{}", service.id)]
    {
        assert!(for_dev.contains(&format!(r#"href="{href}""#)), "{href} in {for_dev}");
    }
    assert!(for_dev.contains(r#"data-search="web shop / production · service""#), "{for_dev}");
    assert!(!for_dev.contains(&blog.id.to_string()) && !for_dev.contains("/settings/servers"), "only what they can open");
    assert!(for_dev.contains("/settings/tokens"));
    let for_admin = web.get("/palette", Some(&ada)).await.body;
    assert!(for_admin.contains(&format!("/projects/{}", blog.id)) && for_admin.contains("/settings/servers"));
    assert_eq!(web.get("/palette", None).await.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn instance_admins_manage_users_and_everyone_their_password() {
    let web = Web::new().await;
    let (admin, _, _, _) = shop_with_team(&web).await;
    let ada = web.sign_in("ada%40example.com").await;
    let dev = web.sign_in("dev%40example.com").await;
    let token = web.get("/", Some(&ada)).await.csrf();
    assert_eq!(web.get("/settings/users", Some(&dev)).await.status, StatusCode::FORBIDDEN);
    let list = web.get("/settings/users", Some(&ada)).await;
    assert!(list.body.contains("dev@example.com") && list.body.contains("Users"));

    let new =
        |password: &str| form(&[("_csrf", &token), ("name", "Grace"), ("email", "grace@example.com"), ("password", password)]);
    let weak = web.post("/settings/users", Some(&ada), &new("short-pw")).await;
    assert_eq!(weak.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(weak.body.contains("Use at least 12 characters") && !weak.body.contains("short-pw"), "the password isn't sent back");
    let reply = web.post("/settings/users", Some(&ada), &new("an initial password")).await;
    let id = created(&reply, "/settings/users/");
    let page = web.get(&format!("/settings/users/{id}"), Some(&ada)).await;
    assert!(page.body.contains("Grace") && page.body.contains("No memberships.") && page.body.contains("Disable user"));

    let account = |name: &str, admin: bool, revision: &str| {
        let mut fields = vec![("_csrf", token.as_str()), ("name", name), ("email", "grace@example.com"), ("revision", revision)];
        if admin {
            fields.push(("admin", "on"));
        }
        form(&fields)
    };
    let saved = web.post(&format!("/settings/users/{id}"), Some(&ada), &account("Grace H.", true, "1")).await;
    assert_eq!(saved.header("location"), Some(format!("/settings/users/{id}?saved=account").as_str()));
    let stale = web.post(&format!("/settings/users/{id}"), Some(&ada), &account("Grace", false, "1")).await;
    assert_eq!(stale.status, StatusCode::UNPROCESSABLE_ENTITY);
    let grace = web.control.get_user(&admin, id.parse().unwrap()).await.unwrap();
    assert_eq!((grace.name.as_str(), grace.admin), ("Grace H.", true));

    // Grace changes her own password; her other sessions end.
    let grace_session = web.sign_in_with("grace%40example.com", "an%20initial%20password").await;
    let other_session = web.sign_in_with("grace%40example.com", "an%20initial%20password").await;
    let grace_token = web.get("/settings/account", Some(&grace_session)).await.csrf();
    let change = |current: &str, new: &str, again: &str| {
        form(&[("_csrf", &grace_token), ("current_password", current), ("new_password", new), ("confirm_password", again)])
    };
    let differ = web
        .post(
            "/settings/account/password",
            Some(&grace_session),
            &change("an initial password", "a new password!", "another one!!"),
        )
        .await;
    assert!(differ.status == StatusCode::UNPROCESSABLE_ENTITY && differ.body.contains("The new passwords differ"));
    let wrong = web
        .post("/settings/account/password", Some(&grace_session), &change("wrong password", "a new password!", "a new password!"))
        .await;
    assert!(wrong.body.contains("It is not your current password"), "{}", wrong.body);
    let changed = web
        .post(
            "/settings/account/password",
            Some(&grace_session),
            &change("an initial password", "a new password!", "a new password!"),
        )
        .await;
    assert_eq!(changed.header("location"), Some("/settings/account?saved=password"));
    assert_eq!(web.get("/settings/account", Some(&grace_session)).await.status, StatusCode::OK);
    assert_eq!(web.get("/settings/account", Some(&other_session)).await.status, StatusCode::SEE_OTHER, "signed out");

    // Her token can be revoked from her page, and disabling her ends her session.
    let (grace_api, _) = web
        .control
        .create_api_token(
            &web.control.authenticate_session(&grace_session).await.unwrap(),
            spinikus_control::NewToken { name: "CI".into(), role: Role::Viewer, scope: None, expires_at: None },
        )
        .await
        .unwrap();
    assert!(web.get(&format!("/settings/users/{id}"), Some(&ada)).await.body.contains("CI"));
    let revoked =
        web.post(&format!("/settings/users/{id}/tokens/{}/revoke", grace_api.id), Some(&ada), &form(&[("_csrf", &token)])).await;
    assert_eq!(revoked.status, StatusCode::SEE_OTHER);
    let disabled = web.post(&format!("/settings/users/{id}/disable"), Some(&ada), &form(&[("_csrf", &token)])).await;
    assert_eq!(disabled.status, StatusCode::SEE_OTHER);
    assert_eq!(web.get("/settings/account", Some(&grace_session)).await.status, StatusCode::SEE_OTHER);
    let page = web.get(&format!("/settings/users/{id}"), Some(&ada)).await;
    assert!(page.body.contains("Enable user") && page.body.contains("No API tokens."));
    let reset = web
        .post(&format!("/settings/users/{id}/password"), Some(&ada), &form(&[("_csrf", &token), ("password", "reset by ada!")]))
        .await;
    assert_eq!(reset.header("location"), Some(format!("/settings/users/{id}?saved=password").as_str()));
    // Admins can't disable themselves: no button, and a refusal if posted anyway.
    let own = web.get(&format!("/settings/users/{}", admin.user.id), Some(&ada)).await;
    assert!(!own.body.contains("Disable user"));
    let refused = web.post(&format!("/settings/users/{}/disable", admin.user.id), Some(&ada), &form(&[("_csrf", &token)])).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(refused.body.contains("You can't disable yourself"), "{}", refused.body);
}

#[tokio::test]
async fn admins_manage_members_and_read_the_audit_log() {
    let web = Web::new().await;
    let (admin, shop, production, _) = shop_with_team(&web).await;
    let ada = web.sign_in("ada%40example.com").await;
    let dev = web.sign_in("dev%40example.com").await;
    let token = web.get("/", Some(&ada)).await.csrf();
    let members = format!("/projects/{}/members", shop.id);
    assert!(web.get(&format!("/projects/{}", shop.id), Some(&ada)).await.body.contains(&members), "the Members tab");
    assert!(!web.get(&format!("/environments/{}", production.id), Some(&dev)).await.body.contains("/members"));
    assert_eq!(web.get(&members, Some(&dev)).await.status, StatusCode::FORBIDDEN);

    let add = |email: &str, role: &str| form(&[("_csrf", &token), ("email", email), ("role", role)]);
    let nobody = web.post(&members, Some(&ada), &add("nobody@example.com", "viewer")).await;
    assert!(nobody.status == StatusCode::UNPROCESSABLE_ENTITY && nobody.body.contains("No user has this email"));
    assert!(nobody.body.contains("nobody@example.com"), "what was typed stays");
    let added = web.post(&members, Some(&ada), &add("dev@example.com", "admin")).await;
    assert_eq!(added.header("location"), Some(format!("{members}?saved=1").as_str()));
    let page = web.get(&members, Some(&dev)).await;
    assert_eq!(page.status, StatusCode::OK, "dev is a project admin now");
    assert!(page.body.contains("dev@example.com") && page.body.contains("(you)"));
    let membership = web.control.list_members(&admin, MembershipScope::Project(shop.id)).await.unwrap().remove(0).membership;
    let scope = format!("project:{}", shop.id);
    let role = form(&[("_csrf", &token), ("role", "viewer"), ("revision", &membership.revision.to_string()), ("scope", &scope)]);
    assert_eq!(web.post(&format!("/memberships/{}", membership.id), Some(&ada), &role).await.status, StatusCode::SEE_OTHER);
    let stale = web.post(&format!("/memberships/{}", membership.id), Some(&ada), &role).await;
    assert!(stale.status == StatusCode::UNPROCESSABLE_ENTITY && stale.body.contains("Someone changed this"));

    // The environment's members, and removing one.
    let environment_members = format!("/environments/{}/members", production.id);
    let page = web.get(&environment_members, Some(&ada)).await;
    assert!(page.body.contains("viewer@example.com") && page.body.contains("have their project role here too"));
    let removed = web
        .post(&format!("/memberships/{}/delete", membership.id), Some(&ada), &form(&[("_csrf", &token), ("scope", &scope)]))
        .await;
    assert_eq!(removed.header("location"), Some(format!("{members}?saved=1").as_str()));
    assert!(web.get(&members, Some(&ada)).await.body.contains("No members yet."));

    // The audit log: the project's part for its admins, everything for instance admins.
    let log = web.get(&format!("/projects/{}/audit-log", shop.id), Some(&ada)).await;
    assert!(log.body.contains("membership.delete") && log.body.contains("by Ada &lt;ada@example.com&gt;"), "{}", log.body);
    assert_eq!(web.get(&format!("/projects/{}/audit-log", shop.id), Some(&dev)).await.status, StatusCode::FORBIDDEN);
    let all = web.get("/settings/audit-log", Some(&ada)).await;
    assert!(all.body.contains("project.create") && all.body.contains(&format!(r#"href="/projects/{}""#, shop.id)));
    assert_eq!(web.get("/settings/audit-log", Some(&dev)).await.status, StatusCode::FORBIDDEN);
    assert_eq!(web.get("/settings/audit-log?before=junk", Some(&ada)).await.status, StatusCode::OK);
}

#[tokio::test]
async fn developers_turn_deploy_on_push_on_and_off() {
    use spinikus_store::WebhookRepository;

    let web = Web::new().await;
    let (admin, _, _, service) = shop_with_team(&web).await;
    let control = &web.control;
    let settings = format!("/services/{}/settings", service.id);
    let session = web.sign_in("dev%40example.com").await;

    // An image service has no such card.
    let page = web.get(&settings, Some(&session)).await;
    assert!(!page.body.contains("Deploy on push"), "{}", page.body);
    let git =
        GitSource { repo_url: "https://github.com/acme/web.git".to_string(), branch: "main".to_string(), root_directory: None };
    control.create_source(&admin, service.id, SourceSpec::Git(git)).await.unwrap();
    let page = web.get(&settings, Some(&session)).await;
    assert!(page.body.contains("Turn on deploy on push") && !page.body.contains("/hooks/git/"), "{}", page.body);

    // Turning it on shows the URLs, what to send, and the secret, once.
    let token = page.csrf();
    let created = web.post(&format!("{settings}/webhook"), Some(&session), &form(&[("_csrf", &token)])).await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.body);
    assert_eq!(created.header("hx-replace-url"), Some(settings.as_str()));
    let webhook = control.store().find_service_webhook(service.id).await.unwrap().unwrap();
    let secret = control.store().webhook_secret(webhook.id).await.unwrap();
    assert!(created.body.contains(secret.expose()), "{}", created.body);
    for provider in ["github", "gitlab", "gitea"] {
        let url = format!("http://spinikus.test/hooks/git/{provider}/{}", webhook.id);
        assert!(created.body.contains(&url), "{url}: {}", created.body);
    }
    assert!(created.body.contains("Pull requests") && created.body.contains("Merge request events"));
    assert!(created.body.contains("none yet"));
    let page = web.get(&settings, Some(&session)).await;
    assert!(page.body.contains(&webhook.paths.github) && !page.body.contains(secret.expose()), "{}", page.body);

    // Behind a proxy that says it took HTTPS, the URLs are HTTPS.
    let proxied = web.send(Method::GET, &settings, Some(&session), &[("x-forwarded-proto", "https")], None).await;
    assert!(proxied.body.contains(&format!("https://spinikus.test{}", webhook.paths.github)), "{}", proxied.body);

    // The last delivery is shown.
    control.store().record_webhook_delivery(webhook.id, "ignored: a push to `other`").await.unwrap();
    let page = web.get(&settings, Some(&session)).await;
    assert!(page.body.contains("ignored: a push to `other`"), "{}", page.body);

    // A new secret keeps the URL.
    let renewed = web.post(&format!("{settings}/webhook"), Some(&session), &form(&[("_csrf", &token)])).await;
    let new_secret = control.store().webhook_secret(webhook.id).await.unwrap();
    assert_ne!(new_secret.expose(), secret.expose());
    assert!(renewed.body.contains(new_secret.expose()) && renewed.body.contains(&webhook.paths.github));

    // A viewer can't open the settings or change the webhook.
    let viewer = web.sign_in("viewer%40example.com").await;
    let viewer_page = web.get("/", Some(&viewer)).await;
    assert_eq!(web.get(&settings, Some(&viewer)).await.status, StatusCode::FORBIDDEN);
    let refused = web.post(&format!("{settings}/webhook/delete"), Some(&viewer), &form(&[("_csrf", &viewer_page.csrf())])).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert!(control.store().find_service_webhook(service.id).await.unwrap().is_some());

    // Turning it off.
    let deleted = web.post(&format!("{settings}/webhook/delete"), Some(&session), &form(&[("_csrf", &token)])).await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER, "{}", deleted.body);
    assert_eq!(deleted.header("location"), Some(format!("{settings}?saved=webhook").as_str()));
    assert!(control.store().find_service_webhook(service.id).await.unwrap().is_none());
    let page = web.get(&format!("{settings}?saved=webhook"), Some(&session)).await;
    assert!(page.body.contains("Turn on deploy on push") && page.body.contains("Saved."), "{}", page.body);
}

#[tokio::test]
async fn developers_add_and_change_volumes_and_admins_remove_them() {
    let web = Web::new().await;
    let (admin, _, production, service) = shop_with_team(&web).await;
    let control = &web.control;
    let settings = format!("/services/{}/settings", service.id);
    let session = web.sign_in("dev%40example.com").await;
    let page = web.get(&settings, Some(&session)).await;
    assert!(page.body.contains("Add volume") && page.body.contains("short downtime"), "{}", page.body);
    let token = page.csrf();

    // Adding one.
    let fields = form(&[("_csrf", &token), ("name", "uploads"), ("mount_path", "/app/uploads/")]);
    let added = web.post(&format!("{settings}/volumes"), Some(&session), &fields).await;
    assert_eq!(added.status, StatusCode::SEE_OTHER, "{}", added.body);
    assert_eq!(added.header("location"), Some(format!("{settings}?saved=volumes").as_str()));
    let volumes = control.list_volumes(&admin, service.id).await.unwrap();
    let uploads = &volumes[0];
    assert_eq!((uploads.name.as_str(), uploads.mount_path.as_str()), ("uploads", "/app/uploads"));
    let page = web.get(&format!("{settings}?saved=volumes"), Some(&session)).await;
    assert!(page.body.contains("/app/uploads") && page.body.contains("Saved."), "{}", page.body);
    // Developers don't get the Remove button.
    assert!(!page.body.contains(&format!("/volumes/{}/delete", uploads.id)), "{}", page.body);

    // A bad one is refused with what was typed.
    let fields = form(&[("_csrf", &token), ("name", "cache"), ("mount_path", "relative/path")]);
    let refused = web.post(&format!("{settings}/volumes"), Some(&session), &fields).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", refused.body);
    assert!(refused.body.contains("must be absolute") && refused.body.contains("relative/path"), "{}", refused.body);

    // Changing one; a refused change keeps what was typed in its row.
    let url = format!("/volumes/{}", uploads.id);
    let revision = uploads.revision.to_string();
    let fields = form(&[("_csrf", &token), ("revision", &revision), ("name", "media"), ("mount_path", "/srv/media")]);
    let changed = web.post(&url, Some(&session), &fields).await;
    assert_eq!(changed.status, StatusCode::SEE_OTHER, "{}", changed.body);
    let media = control.get_volume(&admin, uploads.id).await.unwrap();
    assert_eq!((media.name.as_str(), media.mount_path.as_str()), ("media", "/srv/media"));
    let fields =
        form(&[("_csrf", &token), ("revision", &media.revision.to_string()), ("name", "Bad Name"), ("mount_path", "/srv/media")]);
    let refused = web.post(&url, Some(&session), &fields).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", refused.body);
    assert!(refused.body.contains("value=\"Bad Name\""), "{}", refused.body);
    // A stale revision is explained.
    let stale = web
        .post(
            &url,
            Some(&session),
            &form(&[("_csrf", &token), ("revision", &revision), ("name", "other"), ("mount_path", "/srv/media")]),
        )
        .await;
    assert!(stale.body.contains("Someone changed this"), "{}", stale.body);

    // A viewer can't change them; a developer can't remove them.
    let viewer = web.sign_in("viewer%40example.com").await;
    let viewer_token = web.get("/", Some(&viewer)).await.csrf();
    let fields = form(&[("_csrf", &viewer_token), ("name", "x"), ("mount_path", "/x")]);
    assert_eq!(web.post(&format!("{settings}/volumes"), Some(&viewer), &fields).await.status, StatusCode::FORBIDDEN);
    let removed = web.post(&format!("{url}/delete"), Some(&session), &form(&[("_csrf", &token)])).await;
    assert_eq!(removed.status, StatusCode::FORBIDDEN, "{}", removed.body);

    // An admin removes it.
    let admin_session = web.sign_in("ada%40example.com").await;
    let page = web.get(&settings, Some(&admin_session)).await;
    assert!(page.body.contains(&format!("{url}/delete")), "{}", page.body);
    let removed = web.post(&format!("{url}/delete"), Some(&admin_session), &form(&[("_csrf", &page.csrf())])).await;
    assert_eq!(removed.status, StatusCode::SEE_OTHER, "{}", removed.body);
    assert!(control.list_volumes(&admin, service.id).await.unwrap().is_empty());

    // A database's volume is shown, not edited.
    use spinikus_core::server::ServerFields;
    use spinikus_store::ServerRepository;
    control.store().get_or_create_embedded_server(ServerFields { name: "this machine".to_string() }).await.unwrap();
    let new = spinikus_control::NewDatabase {
        name: "DB".to_string(),
        slug: None,
        engine: Some(spinikus_core::database::Engine::Postgres),
        version: None,
        server_id: None,
        from_backup: None,
        deploy: true,
    };
    let database = control.create_database(&admin, production.id, new).await.unwrap();
    let page = web.get(&format!("/services/{}/settings", database.service.id), Some(&admin_session)).await;
    assert!(
        page.body.contains("Where the database keeps its data") && page.body.contains("/var/lib/postgresql"),
        "{}",
        page.body
    );
    assert!(!page.body.contains("Add volume") && !page.body.contains("/volumes/vol_"), "{}", page.body);
}

#[tokio::test]
async fn instance_admins_schedule_backups_of_spinikus() {
    use spinikus_store::InstanceBackupRepository;

    let web = Web::new().await;
    shop_with_team(&web).await;
    let control = &web.control;
    let ada = web.sign_in("ada%40example.com").await;
    let dev = web.sign_in("dev%40example.com").await;
    assert_eq!(web.get("/settings/backups", Some(&dev)).await.status, StatusCode::FORBIDDEN);

    // Without a destination, the tab says to add one.
    let page = web.get("/settings/backups", Some(&ada)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body.contains(r#"href="/settings/backups""#) && page.body.contains("Add a storage destination"),
        "{}",
        page.body
    );
    let token = page.csrf();
    let admin = control.store().find_user_by_email(&"ada@example.com".parse().unwrap()).await.unwrap().unwrap();
    let principal = spinikus_control::Principal {
        user: admin,
        credential: spinikus_control::Credential::Session(spinikus_core::id::SessionId::new()),
    };
    let new = spinikus_control::NewStorageDestination {
        name: "Offsite".to_string(),
        endpoint: None,
        region: None,
        bucket: "backups".to_string(),
        path_prefix: None,
        path_style: false,
        access_key_id: "AKIA".to_string(),
        secret_access_key: Secret::new("s3cret"),
    };
    let destination = control.create_storage_destination(&principal, new).await.unwrap();

    // Setting the schedule: the passphrase is required, and never shown again.
    let page = web.get("/settings/backups", Some(&ada)).await;
    assert!(
        page.body.contains("Offsite") && page.body.contains("Save schedule") && !page.body.contains("Back up now"),
        "{}",
        page.body
    );
    let schedule = |revision: &str, passphrase: &str| {
        form(&[
            ("_csrf", &token),
            ("revision", revision),
            ("destination_id", &destination.id.to_string()),
            ("cron", "15 2 * * *"),
            ("keep_last", "5"),
            ("enabled", "on"),
            ("passphrase", passphrase),
        ])
    };
    let refused = web.post("/settings/backups/schedule", Some(&ada), &schedule("0", "")).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", refused.body);
    assert!(refused.body.contains("passphrase") && refused.body.contains("15 2 * * *"), "keeps what was typed: {}", refused.body);
    let saved = web.post("/settings/backups/schedule", Some(&ada), &schedule("0", "a long enough passphrase")).await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER, "{}", saved.body);
    let page = web.get("/settings/backups?saved=schedule", Some(&ada)).await;
    assert!(
        page.body.contains("Saved.") && page.body.contains("Next scheduled backup") && page.body.contains("Back up now"),
        "{}",
        page.body
    );
    assert!(!page.body.contains("a long enough passphrase"));
    assert_eq!(control.store().instance_backup_passphrase().await.unwrap().expose(), "a long enough passphrase");
    // An empty passphrase keeps it.
    let kept = web.post("/settings/backups/schedule", Some(&ada), &schedule("1", "")).await;
    assert_eq!(kept.status, StatusCode::SEE_OTHER, "{}", kept.body);
    assert_eq!(control.store().instance_backup_passphrase().await.unwrap().expose(), "a long enough passphrase");

    // Backing up now queues one, which the page follows until it finishes; it can't be deleted yet.
    let queued = web.post("/settings/backups", Some(&ada), &form(&[("_csrf", &token)])).await;
    assert_eq!(queued.status, StatusCode::SEE_OTHER, "{}", queued.body);
    let page = web.get("/settings/backups", Some(&ada)).await;
    assert!(page.body.contains("queued") && page.body.contains(r#"hx-get="/settings/backups/activity""#), "{}", page.body);
    let backup = control.list_instance_backups(&principal, None, 1).await.unwrap().items.remove(0);
    let refused = web.post(&format!("/settings/backups/{}/delete", backup.id), Some(&ada), &form(&[("_csrf", &token)])).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", refused.body);
    assert!(refused.body.contains("hasn&#39;t finished") || refused.body.contains("hasn't finished"), "{}", refused.body);
    assert_eq!(refused.header("hx-replace-url"), Some("/settings/backups"));
    // Without a master key this control plane can't make it: it fails, and then it can go.
    control.advance_instance_backups().await;
    let activity = web.get("/settings/backups/activity", Some(&ada)).await;
    assert!(
        activity.body.contains("failed") && activity.body.contains("no master key") && !activity.body.contains("hx-get"),
        "{}",
        activity.body
    );
    let deleted = web.post(&format!("/settings/backups/{}/delete", backup.id), Some(&ada), &form(&[("_csrf", &token)])).await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER, "{}", deleted.body);
    assert!(web.get("/settings/backups", Some(&ada)).await.body.contains("No backups yet."));
}
