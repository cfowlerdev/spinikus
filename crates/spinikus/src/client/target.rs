//! Finding the project, environment or service a command names: an ID, a slug path such as
//! `shop/production/web`, a slug relative to the directory's link, or the link itself.

use serde_json::Value;

use super::{
    Session,
    error::{Exit, fail},
    views::{self, Environment, Project, Service},
};

impl Session {
    pub async fn project(&self, name: Option<&str>) -> anyhow::Result<Project> {
        Ok(self.project_value(name).await?.1)
    }

    /// The project as the server sent it, and its view.
    pub async fn project_value(&self, name: Option<&str>) -> anyhow::Result<(Value, Project)> {
        let name = name.filter(|name| !name.is_empty());
        let value: Value = match name {
            Some(id) if id.starts_with("prj_") => self.api.get(&format!("/projects/{id}")).await?,
            Some(slug) => {
                let projects: Vec<Value> = self.api.list("/projects").await?;
                find(projects, slug, "project", "")?
            }
            None => {
                let link = self.link()?.ok_or_else(|| needs_link("project"))?;
                self.api.get(&format!("/projects/{}", link.project_id)).await?
            }
        };
        let project = views::read(&value)?;
        Ok((value, project))
    }

    pub async fn environment(&self, name: Option<&str>) -> anyhow::Result<Environment> {
        Ok(self.environment_value(name).await?.1)
    }

    pub async fn environment_value(&self, name: Option<&str>) -> anyhow::Result<(Value, Environment)> {
        let name = name.filter(|name| !name.is_empty());
        let value: Value = match name {
            Some(id) if id.starts_with("env_") => self.api.get(&format!("/environments/{id}")).await?,
            Some(path) => {
                let (project, slug) = match path.split_once('/') {
                    Some((project, slug)) => (self.project(Some(project)).await?, slug),
                    None => (self.project(None).await.map_err(|_| relative_needs_link("environment", path))?, path),
                };
                let environments: Vec<Value> = self.api.list(&format!("/projects/{}/environments", project.id)).await?;
                find(environments, slug, "environment", &project.slug)?
            }
            None => {
                let link = self.link()?.ok_or_else(|| needs_link("environment"))?;
                self.api.get(&format!("/environments/{}", link.environment_id)).await?
            }
        };
        let environment = views::read(&value)?;
        Ok((value, environment))
    }

    pub async fn service(&self, name: Option<&str>) -> anyhow::Result<Service> {
        Ok(self.service_value(name).await?.1)
    }

    pub async fn service_value(&self, name: Option<&str>) -> anyhow::Result<(Value, Service)> {
        let name = name.filter(|name| !name.is_empty());
        let value: Value = match name {
            Some(id) if id.starts_with("svc_") => self.api.get(&format!("/services/{id}")).await?,
            Some(path) => {
                let (environment, slug) = match path.rsplit_once('/') {
                    Some((environment, slug)) => (self.environment(Some(environment)).await?, slug),
                    None => (self.environment(None).await.map_err(|_| relative_needs_link("service", path))?, path),
                };
                let services: Vec<Value> = self.api.list(&format!("/environments/{}/services", environment.id)).await?;
                let within = self.environment_path(&environment).await?;
                find(services, slug, "service", &within)?
            }
            None => {
                let link = self.link()?.ok_or_else(|| needs_link("service"))?;
                let id = link.service_id.as_ref().ok_or_else(|| {
                    fail(Exit::Failure, "name a service: this directory is linked to an environment, not a service")
                })?;
                self.api.get(&format!("/services/{id}")).await?
            }
        };
        let service = views::read(&value)?;
        Ok((value, service))
    }

    /// `project/environment` for an environment.
    pub async fn environment_path(&self, environment: &Environment) -> anyhow::Result<String> {
        let project: Project = views::read(&self.api.get::<Value>(&format!("/projects/{}", environment.project_id)).await?)?;
        Ok(format!("{}/{}", project.slug, environment.slug))
    }

    /// `project/environment/service` for a service.
    pub async fn service_path(&self, service: &Service) -> anyhow::Result<String> {
        let environment = self.environment(Some(&service.environment_id)).await?;
        Ok(format!("{}/{}", self.environment_path(&environment).await?, service.slug))
    }
}

/// The item with this slug (or, failing that, this exact name).
fn find(items: Vec<Value>, slug: &str, what: &str, within: &str) -> anyhow::Result<Value> {
    let field = |item: &Value, key: &str| item.get(key).and_then(Value::as_str).map(str::to_string);
    let at = items
        .iter()
        .position(|item| field(item, "slug").as_deref() == Some(slug))
        .or_else(|| items.iter().position(|item| field(item, "name").as_deref() == Some(slug)));
    if let Some(at) = at {
        return Ok(items.into_iter().nth(at).expect("found above"));
    }
    let place = if within.is_empty() { String::new() } else { format!(" in {within}") };
    let slugs: Vec<String> = items.iter().filter_map(|item| field(item, "slug")).collect();
    let known = if slugs.is_empty() { format!("there are none{place}") } else { format!("there are: {}", slugs.join(", ")) };
    Err(fail(Exit::NotFound, format!("no {what} `{slug}`{place}; {known}")))
}

fn needs_link(what: &str) -> anyhow::Error {
    let article = if what.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" };
    fail(Exit::Failure, format!("name {article} {what}, or run `spinikus link` in this directory first"))
}

fn relative_needs_link(what: &str, name: &str) -> anyhow::Error {
    let example = match what {
        "environment" => "project/environment",
        _ => "project/environment/service",
    };
    fail(Exit::Failure, format!("`{name}` is relative to a linked directory; name the {what} as {example} or by ID"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn finds_by_slug_then_name_and_lists_the_others() {
        let items = || vec![json!({ "slug": "web", "name": "Web" }), json!({ "slug": "api", "name": "web" })];
        assert_eq!(find(items(), "web", "service", "shop/production").unwrap()["slug"], "web");
        assert_eq!(find(items(), "Web", "service", "shop/production").unwrap()["slug"], "web");
        let missing = find(items(), "db", "service", "shop/production").unwrap_err();
        assert_eq!(missing.to_string(), "no service `db` in shop/production; there are: web, api");
        assert_eq!(find(vec![], "shop", "project", "").unwrap_err().to_string(), "no project `shop`; there are none");
    }
}
