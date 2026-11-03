mod audit;
mod backups;
mod databases;
mod deployments;
mod domains;
mod environments;
mod hooks;
mod instance_backups;
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

use serde::{Deserialize, Deserializer};
use spinikus_control::Control;
use utoipa_axum::{router::OpenApiRouter, routes};

/// The API's routes, each with its OpenAPI operation.
pub fn v1() -> OpenApiRouter<Control> {
    OpenApiRouter::new()
        .routes(routes!(crate::openapi::serve))
        .routes(routes!(session::setup_status, session::setup))
        .routes(routes!(session::login))
        .routes(routes!(session::me))
        .routes(routes!(users::change_password))
        .routes(routes!(users::list, users::create))
        .routes(routes!(users::show, users::update))
        .routes(routes!(users::disable))
        .routes(routes!(users::enable))
        .routes(routes!(users::reset_password))
        .routes(routes!(users::tokens))
        .routes(routes!(members::list_project, members::add_project))
        .routes(routes!(members::list_environment, members::add_environment))
        .routes(routes!(members::update, members::delete))
        .routes(routes!(audit::list))
        .routes(routes!(tokens::list, tokens::create))
        .routes(routes!(tokens::revoke))
        .routes(routes!(projects::list, projects::create))
        .routes(routes!(projects::show, projects::update, projects::delete))
        .routes(routes!(environments::list, environments::create))
        .routes(routes!(environments::show, environments::update, environments::delete))
        .routes(routes!(services::list, services::create))
        .routes(routes!(databases::create))
        .routes(routes!(services::show, services::update, services::delete))
        .routes(routes!(sources::show, sources::create, sources::replace))
        .routes(routes!(sources::show_deploy_key, sources::create_deploy_key))
        .routes(routes!(sources::show_webhook, sources::create_webhook, sources::delete_webhook))
        .routes(routes!(deployments::list, deployments::create))
        .routes(routes!(deployments::rollback))
        .routes(routes!(deployments::restart))
        .routes(routes!(service_logs::show))
        .routes(routes!(deployments::show))
        .routes(routes!(deployments::events))
        .routes(routes!(deployments::logs))
        .routes(routes!(deployments::stream))
        .routes(routes!(deployments::cancel))
        .routes(routes!(domains::list, domains::create))
        .routes(routes!(domains::show, domains::update, domains::delete))
        .routes(routes!(variables::list_project, variables::create_project))
        .routes(routes!(variables::list_environment, variables::create_environment))
        .routes(routes!(variables::list_service, variables::create_service))
        .routes(routes!(variables::show, variables::update, variables::delete))
        .routes(routes!(variables::reveal))
        .routes(routes!(volumes::list, volumes::create))
        .routes(routes!(volumes::show, volumes::update, volumes::delete))
        .routes(routes!(backups::list_destinations, backups::create_destination))
        .routes(routes!(backups::show_destination, backups::update_destination, backups::delete_destination))
        .routes(routes!(backups::test_destination))
        .routes(routes!(backups::list_schedules, backups::create_schedule))
        .routes(routes!(backups::show_schedule, backups::update_schedule, backups::delete_schedule))
        .routes(routes!(backups::list, backups::create))
        .routes(routes!(backups::show, backups::delete))
        .routes(routes!(backups::restore))
        .routes(routes!(backups::list_restores))
        .routes(routes!(backups::show_restore))
        .routes(routes!(instance_backups::show_schedule, instance_backups::put_schedule))
        .routes(routes!(instance_backups::list, instance_backups::create))
        .routes(routes!(instance_backups::show, instance_backups::delete))
        .routes(routes!(settings::show, settings::put))
        .routes(routes!(servers::list, servers::create))
        .routes(routes!(servers::show))
        .routes(routes!(servers::revoke))
        .routes(routes!(servers::create_join_token))
}

/// Inbound webhooks, outside `/api/v1`.
pub fn hooks() -> OpenApiRouter<Control> {
    OpenApiRouter::new().routes(routes!(hooks::receive)).layer(axum::extract::DefaultBodyLimit::max(hooks::MAX_DELIVERY_SIZE))
}

/// Tells an explicit `null` (`Some(None)`) apart from a missing field (`None`, by default).
pub(crate) fn present<'de, T: Deserialize<'de>, D: Deserializer<'de>>(deserializer: D) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}
