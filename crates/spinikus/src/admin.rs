use anyhow::{Context, bail};
use spinikus_control::ControlError;
use spinikus_core::{config::SpinikusConfig, secret::Secret};

use crate::{cli::CreateUserArgs, server};

/// Environment variable to pass the password non-interactively, such as from an install script.
const PASSWORD_VAR: &str = "SPINIKUS_ADMIN_PASSWORD";

pub async fn create_user(config: &SpinikusConfig, args: CreateUserArgs) -> anyhow::Result<()> {
    let control = server::open_control(config).await?;
    if !control.needs_setup().await? {
        bail!("Spinikus already has users; add more from the web UI or the API");
    }

    let password = read_password()?;
    match control.create_first_admin(&args.email, &args.name, password).await {
        Ok(user) => {
            println!("Created admin {} ({})", user.email, user.id);
            Ok(())
        }
        Err(error @ ControlError::Invalid { .. }) => bail!("{error}"),
        Err(error) => Err(error).context("Failed to create the admin"),
    }
}

fn read_password() -> anyhow::Result<Secret> {
    if let Ok(password) = std::env::var(PASSWORD_VAR) {
        return Ok(Secret::new(password));
    }
    let password = Secret::new(rpassword::prompt_password("Password: ").context("Failed to read the password")?);
    let confirmation = Secret::new(rpassword::prompt_password("Repeat password: ").context("Failed to read the password")?);
    if password != confirmation {
        bail!("The passwords do not match");
    }
    Ok(password)
}
