use config::{Config, Environment, File};
use spinikus_core::config::{LoggingConfig, SpinikusConfig};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

pub fn init_logging(logging: &LoggingConfig) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&logging.level));

    let registry = tracing_subscriber::registry().with(filter);

    if logging.json {
        registry.with(fmt::layer().json()).init();
    } else {
        registry.with(fmt::layer()).init();
    }
}

pub fn load() -> Result<SpinikusConfig, config::ConfigError> {
    Config::builder()
        .add_source(File::with_name("spinikus.toml"))
        .add_source(Environment::with_prefix("SPINIKUS").prefix_separator("_").separator("__"))
        .build()?
        .try_deserialize()
}
