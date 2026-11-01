mod audit;
mod backups;
mod databases;
mod deployments;
mod domains;
mod environments;
mod hooks;
mod members;
mod projects;
mod servers;
mod service_logs;
mod services;
mod session;
mod settings;
mod sources;
mod tokens;
mod users;
mod variables;
mod volumes;

use axum::{
    Router,
    routing::{get, post},
};
use serde::{Deserialize, Deserializer};
use spinikus_control::Control;

pub fn v1() -> Router<Control> {
    Router::new()
        .route("/setup", get(session::setup_status).post(session::setup))
        .route("/login", post(session::login))
        .route("/me", get(session::me))
        .route("/me/password", axum::routing::put(users::change_password))
        .route("/users", get(users::list).post(users::create))
        .route("/users/{id}", get(users::show).patch(users::update))
        .route("/users/{id}/disable", post(users::disable))
        .route("/users/{id}/enable", post(users::enable))
        .route("/users/{id}/password", axum::routing::put(users::reset_password))
        .route("/users/{id}/tokens", get(users::tokens))
        .route("/projects/{id}/members", get(members::list_project).post(members::add_project))
        .route("/environments/{id}/members", get(members::list_environment).post(members::add_environment))
        .route("/memberships/{id}", axum::routing::patch(members::update).delete(members::delete))
        .route("/audit-log", get(audit::list))
        .route("/tokens", get(tokens::list).post(tokens::create))
        .route("/tokens/{id}", axum::routing::delete(tokens::revoke))
        .route("/projects", get(projects::list).post(projects::create))
        .route("/projects/{id}", get(projects::show).patch(projects::update).delete(projects::delete))
        .route("/projects/{id}/environments", get(environments::list).post(environments::create))
        .route("/environments/{id}", get(environments::show).patch(environments::update).delete(environments::delete))
        .route("/environments/{id}/services", get(services::list).post(services::create))
        .route("/environments/{id}/databases", post(databases::create))
        .route("/services/{id}", get(services::show).patch(services::update).delete(services::delete))
        .route("/services/{id}/source", get(sources::show).post(sources::create).put(sources::replace))
        .route("/services/{id}/source/deploy-key", get(sources::show_deploy_key).post(sources::create_deploy_key))
        .route(
            "/services/{id}/source/webhook",
            get(sources::show_webhook).post(sources::create_webhook).delete(sources::delete_webhook),
        )
        .route("/services/{id}/deployments", get(deployments::list).post(deployments::create))
        .route("/services/{id}/rollback", post(deployments::rollback))
        .route("/services/{id}/logs", get(service_logs::show))
        .route("/deployments/{id}", get(deployments::show))
        .route("/deployments/{id}/events", get(deployments::events))
        .route("/deployments/{id}/logs", get(deployments::logs))
        .route("/deployments/{id}/stream", get(deployments::stream))
        .route("/deployments/{id}/cancel", post(deployments::cancel))
        .route("/services/{id}/domains", get(domains::list).post(domains::create))
        .route("/domains/{id}", get(domains::show).patch(domains::update).delete(domains::delete))
        .route("/projects/{id}/variables", get(variables::list_project).post(variables::create_project))
        .route("/environments/{id}/variables", get(variables::list_environment).post(variables::create_environment))
        .route("/services/{id}/variables", get(variables::list_service).post(variables::create_service))
        .route("/variables/{id}", get(variables::show).patch(variables::update).delete(variables::delete))
        .route("/variables/{id}/reveal", post(variables::reveal))
        .route("/services/{id}/volumes", get(volumes::list).post(volumes::create))
        .route("/volumes/{id}", get(volumes::show).patch(volumes::update).delete(volumes::delete))
        .route("/storage-destinations", get(backups::list_destinations).post(backups::create_destination))
        .route(
            "/storage-destinations/{id}",
            get(backups::show_destination).patch(backups::update_destination).delete(backups::delete_destination),
        )
        .route("/storage-destinations/{id}/test", post(backups::test_destination))
        .route("/services/{id}/backup-schedules", get(backups::list_schedules).post(backups::create_schedule))
        .route(
            "/backup-schedules/{id}",
            get(backups::show_schedule).patch(backups::update_schedule).delete(backups::delete_schedule),
        )
        .route("/services/{id}/backups", get(backups::list).post(backups::create))
        .route("/backups/{id}", get(backups::show).delete(backups::delete))
        .route("/backups/{id}/restore", post(backups::restore))
        .route("/services/{id}/restores", get(backups::list_restores))
        .route("/restores/{id}", get(backups::show_restore))
        .route("/settings/{key}", get(settings::show).put(settings::put))
        .route("/servers", get(servers::list).post(servers::create))
        .route("/servers/{id}", get(servers::show))
        .route("/servers/{id}/revoke", post(servers::revoke))
        .route("/servers/{id}/enrollment-tokens", post(servers::create_join_token))
}

/// Inbound webhooks, outside `/api/v1`.
pub fn hooks() -> Router<Control> {
    Router::new()
        .route("/git/{provider}/{id}", post(hooks::receive))
        .layer(axum::extract::DefaultBodyLimit::max(hooks::MAX_DELIVERY_SIZE))
}

/// Tells an explicit `null` (`Some(None)`) apart from a missing field (`None`, by default).
pub(crate) fn present<'de, T: Deserialize<'de>, D: Deserializer<'de>>(deserializer: D) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}
