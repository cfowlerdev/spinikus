mod deployments;
mod domains;
mod environments;
mod projects;
mod servers;
mod services;
mod session;
mod settings;
mod sources;

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
        .route("/projects", get(projects::list).post(projects::create))
        .route("/projects/{id}", get(projects::show).patch(projects::update).delete(projects::delete))
        .route("/projects/{id}/environments", get(environments::list).post(environments::create))
        .route("/environments/{id}", get(environments::show).patch(environments::update).delete(environments::delete))
        .route("/environments/{id}/services", get(services::list).post(services::create))
        .route("/services/{id}", get(services::show).patch(services::update).delete(services::delete))
        .route("/services/{id}/source", get(sources::show).post(sources::create).put(sources::replace))
        .route("/services/{id}/source/deploy-key", get(sources::show_deploy_key).post(sources::create_deploy_key))
        .route("/services/{id}/deployments", get(deployments::list).post(deployments::create))
        .route("/deployments/{id}", get(deployments::show))
        .route("/deployments/{id}/events", get(deployments::events))
        .route("/deployments/{id}/logs", get(deployments::logs))
        .route("/deployments/{id}/cancel", post(deployments::cancel))
        .route("/services/{id}/domains", get(domains::list).post(domains::create))
        .route("/domains/{id}", get(domains::show).patch(domains::update).delete(domains::delete))
        .route("/settings/{key}", get(settings::show).put(settings::put))
        .route("/servers", get(servers::list).post(servers::create))
        .route("/servers/{id}", get(servers::show))
        .route("/servers/{id}/revoke", post(servers::revoke))
        .route("/servers/{id}/enrollment-tokens", post(servers::create_join_token))
}

/// Tells an explicit `null` (`Some(None)`) apart from a missing field (`None`, by default).
pub(crate) fn present<'de, T: Deserialize<'de>, D: Deserializer<'de>>(deserializer: D) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}
