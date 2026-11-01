//! The dashboard over HTTP requests, against a real service layer and database.

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Method, Request, StatusCode, header},
};
use http_body_util::BodyExt;
use spinikus_control::{Control, NewEnvironment, NewProject, NewService, password};
use spinikus_core::{
    environment::EnvironmentKind,
    membership::{MembershipScope, Role},
    secret::Secret,
    service::{ServiceConfig, ServiceKind},
    user::UserFields,
};
use spinikus_store::{MasterKey, MembershipRepository, SqliteStore, UserRepository, connect, migrate};
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
        let control = Control::new(store);
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
