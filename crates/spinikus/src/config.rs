use config::{Config, Environment, File};
use serde::Deserialize;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};


#[derive(Deserialize, Default)]
pub struct LoggingConfig {
    pub level: String,
    pub json: bool
}

impl LoggingConfig {
    pub fn init(&self) {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&self.level));

        let registry = tracing_subscriber::registry().with(filter);

        if self.json {
            registry.with(fmt::layer().json()).init();
        } else {
            registry.with(fmt::layer()).init();
        }
    }
}


#[derive(Deserialize, Default)]
pub struct SpinikusConfig {
    pub logging: LoggingConfig
}

impl SpinikusConfig {
    pub fn load() -> Result<Self, config::ConfigError> {
        Config::builder()
            .add_source(File::with_name("spinikus.toml"))
            .add_source(Environment::with_prefix("SPINIKUS").separator("__"))
            .build()?
            .try_deserialize()
    }
}
