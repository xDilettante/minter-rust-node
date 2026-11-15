use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand, ValueEnum};
use minter_node_control::{Controller, Mode, Settings};
use std::{path::PathBuf, time::Duration};

#[derive(Subcommand)]
pub(crate) enum Action {
    /// Create settings without replacing an existing file. Edit them or open the GUI before running.
    Init {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        data_dir: PathBuf,
        #[arg(long, value_enum, default_value = "light")]
        mode: InitialMode,
    },
    /// Show shared settings, migrating the old desktop format in memory.
    Settings(Config),
    /// Observe a local controller, full node or saved light header.
    Status(Config),
    /// Stop this user's controller for the configured data directory and await database closure.
    Stop(Config),
    /// Show recent lines from the node log.
    Logs(Config),
    /// Supervise a full or light node in the foreground. Ctrl-C stops it.
    Run {
        #[command(flatten)]
        config: Config,
        /// Stop when the parent closes standard input.
        #[arg(long)]
        shutdown_on_stdin_close: bool,
    },
}

#[derive(Args)]
pub(crate) struct Config {
    #[arg(long)]
    config: PathBuf,
}

impl Config {
    fn controller(self) -> Result<Controller> {
        let config = std::path::absolute(self.config)?;
        ensure!(
            config.is_file(),
            "Файл настроек отсутствует; создайте его через node init или GUI"
        );
        let data = config
            .parent()
            .context("Каталог настроек отсутствует")?
            .join("node");
        Controller::new(config, data)
    }
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum InitialMode {
    Light,
    Full,
}

pub(crate) fn run(action: Action) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        match action {
            Action::Init { config, data_dir, mode } => {
                let mut settings = Settings::defaults(std::path::absolute(data_dir)?);
                settings.mode = match mode { InitialMode::Light => Mode::Light, InitialMode::Full => Mode::Full };
                settings.save_new(&std::path::absolute(config)?)?;
                println!("{}", serde_json::to_string_pretty(&settings)?);
            }
            Action::Settings(config) => println!("{}", serde_json::to_string_pretty(&config.controller()?.settings()?)?),
            Action::Status(config) => println!("{}", serde_json::to_string_pretty(&config.controller()?.status().await?)?),
            Action::Stop(config) => config.controller()?.stop().await?,
            Action::Logs(config) => println!("{}", config.controller()?.logs()?),
            Action::Run { config, shutdown_on_stdin_close } => {
                let controller = config.controller()?;
                controller.start().await?;
                eprintln!("Нода запущена. Для остановки нажмите Ctrl-C. Журнал: node.log в папке данных.");
                let outcome = tokio::select! {
                    result = shutdown_signal(shutdown_on_stdin_close) => result.context("Ошибка ожидания остановки"),
                    result = async {
                        loop {
                            if !controller.status().await?.managed { return Ok::<_, anyhow::Error>(()); }
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }
                    } => result,
                };
                // Always join the child, including an error reading local status.
                let stopped = controller.stop_owned().await;
                outcome?;
                stopped?;
            }
        }
        Ok(())
    })
}

async fn shutdown_signal(on_stdin_close: bool) -> std::io::Result<()> {
    let signal = async {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut terminate = signal(SignalKind::terminate())?;
            tokio::select! {
                result = tokio::signal::ctrl_c() => result,
                _ = terminate.recv() => Ok(()),
            }
        }
        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c().await
        }
    };
    let stdin = async {
        if !on_stdin_close {
            std::future::pending::<()>().await;
        }
        let (send, receive) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("node-parent".into())
            .spawn(move || {
                use std::io::Read;
                let result = std::io::copy(
                    &mut std::io::stdin().lock().take(u64::MAX),
                    &mut std::io::sink(),
                )
                .map(|_| ());
                let _ = send.send(result);
            })?;
        receive.await.map_err(std::io::Error::other)?
    };
    tokio::select! { result = signal => result, result = stdin => result }
}
