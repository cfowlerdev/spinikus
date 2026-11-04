use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::Context;
use spinikus_agent::ProxyListeners;
use spinikus_control::Control;
use spinikus_core::{config::SpinikusConfig, secret::Secret};
use spinikus_store::SqliteStore;
use sqlx::SqlitePool;
use tokio::{net::TcpListener, sync::watch};

use crate::{cli::ServerRunArgs, secrets};

/// Where to connect to a listener on this machine: its own address, or loopback for one
/// bound to every address.
fn loopback(address: SocketAddr) -> SocketAddr {
    match address.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => SocketAddr::from((Ipv4Addr::LOCALHOST, address.port())),
        IpAddr::V6(ip) if ip.is_unspecified() => SocketAddr::from((Ipv6Addr::LOCALHOST, address.port())),
        _ => address,
    }
}

pub async fn run(config: &SpinikusConfig, args: ServerRunArgs) -> anyhow::Result<()> {
    let _lock = lock_instance(config)?;
    let control = with_setup_code(config, open_control(config).await?).await?;

    let http_address = args.listen.unwrap_or(config.http.listen);
    let agent_address = args.agent_listen.unwrap_or(config.agents.listen);
    let http_listener = TcpListener::bind(http_address).await.with_context(|| format!("Failed to listen on {http_address}"))?;
    let http_listener_address = http_listener.local_addr().context("Failed to read the HTTP listener's address")?;
    let agent_listener =
        TcpListener::bind(agent_address).await.with_context(|| format!("Failed to listen for agents on {agent_address}"))?;

    let (stop, stopped) = watch::channel(false);
    // The API under /api/v1 and /hooks, the dashboard everywhere else.
    let router = spinikus_api::router(control.clone()).merge(spinikus_web::router(control.clone()));
    let http = axum::serve(http_listener, router.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(until(stopped.clone()));
    let agent_address = agent_listener.local_addr().context("Failed to read the agent listener's address")?;
    let proxy = match config.agents.embedded {
        // The embedded agent's proxy serves the dashboard on its domain, if one is set.
        true => crate::agent::proxy_listeners(config)
            .await?
            .map(|listeners| ProxyListeners { dashboard: Some(loopback(http_listener_address)), ..listeners }),
        false => None,
    };
    let embedded = config.agents.embedded.then(|| {
        let (control, files, shutdown) = (control.clone(), crate::agent::files(config), until(stopped.clone()));
        tokio::spawn(async move {
            let run = crate::embedded::run(control, files, agent_address, crate::agent::runtime(), proxy, shutdown);
            // The control plane keeps running without it; the error says why it stopped.
            if let Err(error) = run.await {
                tracing::error!("{error:#}");
            }
        })
    });
    let deployments = {
        let (control, shutdown) = (control.clone(), until(stopped.clone()));
        tokio::spawn(async move { control.run_deployments(shutdown).await })
    };
    let certificates = {
        let (control, shutdown) = (control.clone(), until(stopped.clone()));
        tokio::spawn(async move { control.run_certificates(shutdown).await })
    };
    let backups = {
        let (control, shutdown) = (control.clone(), until(stopped.clone()));
        tokio::spawn(async move { control.run_backups(shutdown).await })
    };
    let instance_backups = {
        let (control, shutdown) = (control.clone(), until(stopped.clone()));
        tokio::spawn(async move { control.run_instance_backups(shutdown).await })
    };
    let notifications = {
        let (control, shutdown) = (control.clone(), until(stopped.clone()));
        tokio::spawn(async move { control.run_notifications(shutdown).await })
    };
    let metrics = {
        let (control, shutdown) = (control.clone(), until(stopped.clone()));
        tokio::spawn(async move { control.run_metrics(shutdown).await })
    };
    let agents = spinikus_control::agents::serve(control, agent_listener, until(stopped));
    tracing::info!(dashboard = %format!("http://{http_address}/"), "Server started");

    let mut http = tokio::spawn(async move { http.await.context("HTTP server failed") });
    let mut agents =
        tokio::spawn(async move { agents.await.map_err(|error| anyhow::anyhow!(error)).context("Agent listener failed") });

    // Run until Ctrl+C or until either listener stops, then stop both and wait for them.
    let first = tokio::select! {
        signal = crate::shutdown::signal() => Stopped::Signal(signal),
        result = &mut http => Stopped::Http(joined(result)),
        result = &mut agents => Stopped::Agents(joined(result)),
    };
    let _ = stop.send(true);
    let result = match first {
        Stopped::Signal(signal) => {
            tracing::info!("Shutting down");
            let (http, agents) = tokio::join!(http, agents);
            signal.context("Failed to listen for the shutdown signal").and(joined(http)).and(joined(agents))
        }
        Stopped::Http(result) => result.and(joined(agents.await)),
        Stopped::Agents(result) => result.and(joined(http.await)),
    };
    let _ = deployments.await;
    let _ = certificates.await;
    let _ = backups.await;
    let _ = instance_backups.await;
    let _ = notifications.await;
    let _ = metrics.await;
    if let Some(embedded) = embedded {
        let _ = embedded.await;
    }
    tracing::info!("Shut down");
    result
}

/// What ended the server first.
enum Stopped {
    Signal(std::io::Result<()>),
    Http(anyhow::Result<()>),
    Agents(anyhow::Result<()>),
}

fn joined(result: Result<anyhow::Result<()>, tokio::task::JoinError>) -> anyhow::Result<()> {
    result.context("Listener task panicked")?
}

/// Resolves once `stopped` turns true.
async fn until(mut stopped: watch::Receiver<bool>) {
    let _ = stopped.wait_for(|stopped| *stopped).await;
}

/// The file `server run` holds a lock on, in the data directory.
pub const LOCK_FILE: &str = "spinikus.lock";

/// Locks the instance's data directory for as long as the file returned is open: `server run`
/// holds it, so `admin restore` never swaps the database out from under a running server.
/// The lock goes with the process, however it ends.
pub fn lock_instance(config: &SpinikusConfig) -> anyhow::Result<std::fs::File> {
    let path = config.data_dir.join(LOCK_FILE);
    std::fs::create_dir_all(&config.data_dir).with_context(|| format!("Failed to create {}", config.data_dir.display()))?;
    // Opened for reading when it exists, so any user that can read it can lock it.
    let file = match std::fs::File::open(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::OpenOptions::new().append(true).create(true).open(&path)
        }
        opened => opened,
    }
    .with_context(|| format!("Failed to open {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
            "Spinikus is running with the data in {}; stop it first (sudo systemctl stop spinikus)",
            config.data_dir.display()
        ),
        Err(std::fs::TryLockError::Error(error)) => Err(error).with_context(|| format!("Failed to lock {}", path.display())),
    }
}

/// The file with the setup code, in the data directory.
pub const SETUP_CODE_FILE: &str = "setup-code";

/// Until there is a user, the setup page needs the code in `<data_dir>/setup-code`, made on
/// the first start (readable only by the instance's user) and printed by the install script.
/// Once there are users the file is removed.
async fn with_setup_code(config: &SpinikusConfig, control: Control) -> anyhow::Result<Control> {
    let path = config.data_dir.join(SETUP_CODE_FILE);
    if !control.needs_setup().await? {
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::info!(path = %path.display(), "Removed the setup code, since Spinikus is set up"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(path = %path.display(), %error, "Failed to remove the setup code"),
        }
        return Ok(control);
    }
    let code = match std::fs::read_to_string(&path) {
        Ok(code) if !code.trim().is_empty() => Secret::new(code.trim().to_string()),
        Ok(_) | Err(_) => {
            let code = Control::new_setup_code();
            write_private(&path, code.expose())
                .with_context(|| format!("Failed to write the setup code to {}", path.display()))?;
            code
        }
    };
    tracing::warn!(
        code_file = %path.display(),
        "No users yet: open /setup in the dashboard with the setup code from the file, or use `spinikus admin create-user`"
    );
    Ok(control.with_setup_code(code))
}

/// Writes a file only its owner can read.
fn write_private(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())?;
    file.write_all(b"\n")
}

pub async fn migrate(config: &SpinikusConfig) -> anyhow::Result<()> {
    open_database(config).await?;
    Ok(())
}

/// Opens the database (applying migrations) and the secrets keyring, and builds the service layer.
pub async fn open_control(config: &SpinikusConfig) -> anyhow::Result<Control> {
    let pool = open_database(config).await?;
    let master_key = secrets::load_master_key(config)?;
    let store = SqliteStore::open(pool, &master_key).await.context("Failed to load the secrets keyring")?;
    tracing::info!(master_key = master_key.id(), "Secrets keyring ready");
    // Instance backups are written in the data directory, which has room for the database.
    Ok(Control::new(store).with_log_dir(config.data_dir.join("logs")).with_instance_backups(master_key, config.data_dir.clone()))
}

/// Connects to the database and applies any pending migrations.
pub(crate) async fn open_database(config: &SpinikusConfig) -> anyhow::Result<SqlitePool> {
    let path = config.database_path();
    let pool = spinikus_store::connect(&path).await.with_context(|| format!("Failed to open database at {}", path.display()))?;

    spinikus_store::migrate(&pool).await.context("Failed to apply database migrations")?;
    tracing::info!(path = %path.display(), "Database ready");
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use spinikus_core::{config::SpinikusConfig, secret::Secret};

    use super::{SETUP_CODE_FILE, loopback, open_control, with_setup_code};

    #[test]
    fn connects_to_any_address_listeners_over_loopback() {
        let address = |value: &str| value.parse::<std::net::SocketAddr>().unwrap();
        assert_eq!(loopback(address("0.0.0.0:8080")), address("127.0.0.1:8080"));
        assert_eq!(loopback(address("[::]:8080")), address("[::1]:8080"));
        assert_eq!(loopback(address("10.0.0.5:8080")), address("10.0.0.5:8080"));
    }

    #[tokio::test]
    async fn keeps_a_setup_code_until_there_is_a_user() {
        let dir = tempfile::tempdir().unwrap();
        let config = SpinikusConfig { data_dir: dir.path().to_path_buf(), ..Default::default() };
        let path = dir.path().join(SETUP_CODE_FILE);

        let control = with_setup_code(&config, open_control(&config).await.unwrap()).await.unwrap();
        assert!(control.setup_code_required());
        let code = std::fs::read_to_string(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // A restart keeps the code the install script printed.
        let control = with_setup_code(&config, open_control(&config).await.unwrap()).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), code);

        control.set_up(code.trim(), "ada@example.com", "Ada", Secret::new("correct horse battery staple")).await.unwrap();
        let control = with_setup_code(&config, open_control(&config).await.unwrap()).await.unwrap();
        assert!(!control.setup_code_required());
        assert!(!path.exists(), "removed once Spinikus is set up");
    }
}
