mod logic;
mod ui;

use color_eyre::{Result, eyre::Context};
use tokio::fs::File;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_writer(
            File::create("log")
                .await
                .wrap_err("Failed to create log file")?
                .into_std()
                .await,
        )
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let terminal = ratatui::init();
    let result = logic::run(terminal).await;
    ratatui::restore();

    result
}

#[derive(Default, PartialEq, Eq)]
enum List {
    #[default]
    Unpaired,
    Paired,
}

impl std::fmt::Display for List {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            List::Unpaired => write!(f, "Unpaired"),
            List::Paired => write!(f, "Paired"),
        }
    }
}

#[derive(Debug)]
struct Device {
    pub alias: String,
    pub connected: bool,
    pub loading: Option<usize>,
}

#[derive(Clone)]
struct Error {
    pub message: String,
    pub process: String,
}
