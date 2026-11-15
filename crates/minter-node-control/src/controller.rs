use crate::settings::{FullStart, Mode, Settings, StorageProfile};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::{Mutex as AsyncMutex, watch},
    task::JoinHandle,
};

const LOG_LIMIT: u64 = 2 * 1024 * 1024;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeStatus {
    pub network: String,
    pub height: String,
    pub app_hash: String,
    pub block_time: String,
    pub version: String,
    pub catching_up: bool,
    pub stale: bool,
    pub trust_expires_at: Option<String>,
}

impl NodeStatus {
    fn refresh_expiration(&mut self, now: time::OffsetDateTime) -> Result<()> {
        if let Some(expires) = &self.trust_expires_at {
            let expires = time::OffsetDateTime::parse(
                expires,
                &time::format_description::well_known::Rfc3339,
            )
            .context("Неверный срок доверия заголовка")?;
            self.stale |= now >= expires;
        }
        Ok(())
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub phase: String,
    pub managed: bool,
    #[serde(default)]
    pub controllable: bool,
    #[serde(default)]
    pub control_id: Option<String>,
    pub node: Option<NodeStatus>,
    pub error: Option<String>,
    #[serde(default)]
    pub previous_error: Option<String>,
    pub free_bytes: Option<u64>,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            phase: "stopped".into(),
            managed: false,
            controllable: false,
            control_id: None,
            node: None,
            error: None,
            previous_error: None,
            free_bytes: None,
        }
    }
}

struct Active {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

pub struct Controller {
    config: PathBuf,
    settings: Mutex<Settings>,
    state: Arc<Mutex<Status>>,
    full_node_id: Arc<Mutex<Option<String>>>,
    active: AsyncMutex<Option<Active>>,
    client: reqwest::Client,
}

impl Controller {
    pub fn new(config: PathBuf, data_dir: PathBuf) -> Result<Self> {
        let config = if config.exists() {
            config.canonicalize()?
        } else {
            std::path::absolute(config)?
        };
        let settings = Settings::load(&config, data_dir)?;
        Ok(Self {
            config,
            settings: Mutex::new(settings),
            state: Arc::new(Mutex::new(Status::default())),
            full_node_id: Arc::new(Mutex::new(None)),
            active: AsyncMutex::new(None),
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(3))
                .build()?,
        })
    }

    pub fn settings(&self) -> Result<Settings> {
        Ok(self
            .settings
            .lock()
            .map_err(|_| anyhow::anyhow!("Настройки заняты"))?
            .clone())
    }

    pub async fn save(&self, settings: Settings) -> Result<()> {
        let active = self.active.lock().await;
        ensure!(
            active.as_ref().is_none_or(|a| a.task.is_finished()),
            "Сначала остановите ноду"
        );
        let _lease = configuration_lock(&self.config)?;
        settings.save(&self.config)?;
        *self
            .settings
            .lock()
            .map_err(|_| anyhow::anyhow!("Настройки заняты"))? = settings;
        *self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Статус занят"))? = Status::default();
        Ok(())
    }

    pub async fn start(&self) -> Result<()> {
        let mut active = self.active.lock().await;
        ensure!(
            active.as_ref().is_none_or(|a| a.task.is_finished()),
            "Нода уже запущена"
        );
        let lease = configuration_lock(&self.config)?;
        let settings = Settings::load(&self.config, self.settings()?.data_dir)?;
        settings.validate(true)?;
        *self
            .settings
            .lock()
            .map_err(|_| anyhow::anyhow!("Настройки заняты"))? = settings.clone();
        let binary = executable(if settings.mode == Mode::Full {
            "minterd"
        } else {
            "minterctl"
        })?;
        fs::create_dir_all(&settings.data_dir).context("Не удалось создать папку данных")?;
        let data_lease = exclusive_lock(&settings.data_dir.canonicalize()?.join("controller.lock"))
            .context("Папка данных уже используется другим процессом управления нодой")?;
        let log = Arc::new(Mutex::new(Log::open(&settings.data_dir.join("node.log"))?));
        let server = crate::ipc::Server::bind(&settings.data_dir, settings.mode).await?;
        let state = self.state.clone();
        let full_node_id = self.full_node_id.clone();
        *full_node_id
            .lock()
            .map_err(|_| anyhow::anyhow!("Статус занят"))? = None;
        *state.lock().map_err(|_| anyhow::anyhow!("Статус занят"))? = Status {
            phase: "starting".into(),
            managed: true,
            controllable: true,
            control_id: Some(server.id().to_owned()),
            ..Default::default()
        };
        let (stop, stopped) = watch::channel(false);
        let remote_stop = stop.clone();
        let client = self.client.clone();
        let task = tokio::spawn(async move {
            let running = async {
                if settings.mode == Mode::Full {
                    run_full(
                        &settings,
                        &binary,
                        log.clone(),
                        stopped,
                        full_node_id.clone(),
                    )
                    .await
                } else {
                    run_light(&settings, &binary, log.clone(), stopped, state.clone()).await
                }
            };
            let (result, pending) = server
                .serve_until(
                    running,
                    || observe(&settings, &state, &client, &full_node_id),
                    &remote_stop,
                )
                .await;
            {
                let mut status = state.lock().unwrap();
                status.managed = false;
                status.controllable = false;
                match &result {
                    Ok(()) => {
                        status.phase = "stopped".into();
                        status.error = None;
                    }
                    Err(error) => {
                        let error = format!("{error:#}");
                        if let Ok(mut log) = log.lock() {
                            let _ = log.append(format!("\n{error}\n").as_bytes());
                        }
                        status.phase = "error".into();
                        status.error = Some(error);
                    }
                }
            }
            // serve_until already closed/unlinked its listener. Release both
            // leases before acknowledging a remote handoff to a new owner.
            drop(log);
            drop(data_lease);
            drop(lease);
            if let Some(pending) = pending
                && let Err(error) = pending.finish(&result).await
            {
                eprintln!("Не удалось передать результат остановки: {error:#}");
            }
        });
        *active = Some(Active { stop, task });
        Ok(())
    }

    pub async fn stop(&self) -> Result<()> {
        self.stop_target(None).await
    }

    /// Stop only this instance's child. Owner cleanup must never target a replacement.
    pub async fn stop_owned(&self) -> Result<()> {
        let mut active = self.active.lock().await;
        stop_active(&mut active, &self.state).await
    }

    /// Bind a user action to the generation actually displayed by its client.
    pub async fn stop_target(&self, expected: Option<&str>) -> Result<()> {
        let mut active = self.active.lock().await;
        if active.is_some() {
            if let Some(expected) = expected {
                ensure!(
                    self.state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Статус занят"))?
                        .control_id
                        .as_deref()
                        == Some(expected),
                    "Владелец ноды изменился; обновите статус"
                );
            }
            return stop_active(&mut active, &self.state).await;
        }
        drop(active);
        let settings = self.settings()?;
        let instance = match expected {
            Some(expected) => expected.to_owned(),
            None => {
                let reply = crate::ipc::request(
                    &settings.data_dir,
                    settings.mode,
                    crate::ipc::Command::Status,
                    None,
                )
                .await?;
                let Some(crate::ipc::Reply::Status(status)) = reply else {
                    bail!(
                        "Не найден локальный контроллер этой папки данных; остановка не подтверждена"
                    )
                };
                status.control_id.context("Нет ID локального контроллера")?
            }
        };
        let reply = crate::ipc::request(
            &settings.data_dir,
            settings.mode,
            crate::ipc::Command::Stop,
            Some(&instance),
        )
        .await?;
        ensure!(
            matches!(reply, Some(crate::ipc::Reply::Stopped)),
            "Локальный контроллер исчез; остановка не подтверждена"
        );
        Ok(())
    }

    pub async fn status(&self) -> Result<Status> {
        let mut operation = self.active.lock().await;
        if operation
            .as_ref()
            .is_some_and(|active| active.task.is_finished())
        {
            operation
                .take()
                .expect("finished controller")
                .task
                .await
                .context("Процесс управления завершился с ошибкой")?;
        }
        let settings = self.settings()?;
        let local = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Статус занят"))?
            .clone();
        if !local.managed {
            match crate::ipc::request(
                &settings.data_dir,
                settings.mode,
                crate::ipc::Command::Status,
                None,
            )
            .await
            {
                Ok(Some(crate::ipc::Reply::Status(mut status))) => {
                    status.managed = false;
                    status.previous_error = local.error.clone();
                    if matches!(status.phase.as_str(), "running" | "syncing") {
                        status.phase = "attached".into();
                    }
                    return Ok(*status);
                }
                Ok(Some(_)) => bail!("Неверный ответ статуса контроллера"),
                Err(error) if local.phase != "error" => return Err(error),
                _ => {}
            }
        }
        observe(&settings, &self.state, &self.client, &self.full_node_id).await
    }

    pub fn logs(&self) -> Result<String> {
        let path = self.settings()?.data_dir.join("node.log");
        let Ok(mut file) = File::open(path) else {
            return Ok(String::new());
        };
        let size = file.metadata()?.len();
        file.seek(SeekFrom::Start(size.saturating_sub(64 * 1024)))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let text = String::from_utf8_lossy(&bytes);
        let lines = text.lines().rev().take(120).collect::<Vec<_>>();
        Ok(lines.into_iter().rev().collect::<Vec<_>>().join("\n"))
    }
}

async fn stop_active(active: &mut Option<Active>, state: &Mutex<Status>) -> Result<()> {
    if let Some(running) = active.take() {
        running.stop.send_replace(true);
        running
            .task
            .await
            .context("Процесс управления завершился с ошибкой")?;
    }
    let status = state.lock().map_err(|_| anyhow::anyhow!("Статус занят"))?;
    ensure!(
        status.phase != "error",
        "{}",
        status.error.as_deref().unwrap_or("Ошибка завершения ноды")
    );
    Ok(())
}

async fn observe(
    settings: &Settings,
    state: &Mutex<Status>,
    client: &reqwest::Client,
    full_node_id: &Mutex<Option<String>>,
) -> Result<Status> {
    let mut status = state
        .lock()
        .map_err(|_| anyhow::anyhow!("Статус занят"))?
        .clone();
    let generation = status.control_id.clone();
    status.controllable = status.managed;
    if !status.managed {
        status.control_id = None;
    }
    let expected_id = full_node_id
        .lock()
        .map_err(|_| anyhow::anyhow!("Статус занят"))?
        .clone();
    if settings.mode == Mode::Full
        && status.phase != "error"
        && (!status.managed || expected_id.is_some())
    {
        let observed = read_full_status(
            client,
            settings.api_port,
            expected_id.as_deref().filter(|_| status.managed),
        )
        .await;
        let mut shared = state.lock().map_err(|_| anyhow::anyhow!("Статус занят"))?;
        if shared.managed != status.managed
            || shared.control_id != generation
            || shared.phase == "error"
        {
            // The worker may have finished while this HTTP request was pending.
            status = shared.clone();
            status.controllable = status.managed;
            if !status.managed {
                status.control_id = None;
            }
        } else {
            match observed {
                Ok(node) => {
                    status.node = Some(node);
                    shared.node = status.node.clone();
                    status.phase = if status.managed {
                        if status.node.as_ref().is_some_and(|node| node.catching_up) {
                            "syncing"
                        } else {
                            "running"
                        }
                    } else {
                        "attached"
                    }
                    .into();
                    status.error = None;
                }
                Err(error) if status.managed => {
                    status.error = Some(format!("Локальный API пока недоступен: {error:#}"));
                }
                _ => {}
            }
        }
    } else if settings.mode == Mode::Light
        && status.phase != "error"
        && !status.managed
        && status.node.is_none()
        && settings.data_dir.join("light.redb").is_file()
    {
        // Even `light status` opens redb exclusively. Hold the same lease as
        // start so an observer cannot win the gap before IPC publication.
        let _data_lease =
            match exclusive_lock(&settings.data_dir.canonicalize()?.join("controller.lock")) {
                Ok(lease) => lease,
                Err(error)
                    if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                        error.kind() == std::io::ErrorKind::WouldBlock
                            || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
                    }) =>
                {
                    status.phase = "attached".into();
                    status.error = Some(
                        "Папка данных занята. Локальный канал управления пока недоступен.".into(),
                    );
                    status.free_bytes = fs2::available_space(&settings.data_dir).ok();
                    return Ok(status);
                }
                Err(error) => return Err(error),
            };
        let mut command = Command::new(executable("minterctl")?);
        command
            .args(["light", "status"])
            .arg("--db")
            .arg(settings.data_dir.join("light.redb"))
            .kill_on_drop(true);
        prepare_child(&mut command);
        let output = tokio::time::timeout(Duration::from_secs(5), command.output())
            .await
            .context("Истекло время чтения сохранённого заголовка")??;
        ensure!(
            output.status.success(),
            "Не удалось прочитать light.redb {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        status.node = Some(light_status(&value)?);
        state
            .lock()
            .map_err(|_| anyhow::anyhow!("Статус занят"))?
            .node = status.node.clone();
    }
    if let Some(node) = &mut status.node {
        node.refresh_expiration(time::OffsetDateTime::now_utc())?;
    }
    let mut folder = settings.data_dir.as_path();
    while !folder.exists() {
        folder = folder.parent().context("Не удалось найти диск данных")?;
    }
    status.free_bytes = fs2::available_space(folder).ok();
    Ok(status)
}

fn configuration_lock(config: &Path) -> Result<File> {
    let parent = config.parent().context("Каталог настроек отсутствует")?;
    fs::create_dir_all(parent)?;
    let mut path = if config.exists() {
        config.canonicalize()?
    } else {
        parent
            .canonicalize()?
            .join(config.file_name().context("Имя настроек отсутствует")?)
    };
    let mut name = path.as_os_str().to_os_string();
    name.push(".lock");
    path = name.into();
    exclusive_lock(&path).context("Настройки уже используются другим процессом управления нодой")
}

fn exclusive_lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    fs2::FileExt::try_lock_exclusive(&file)?;
    Ok(file)
}

fn executable(name: &str) -> Result<PathBuf> {
    let suffix = std::env::consts::EXE_SUFFIX;
    let bundled = std::env::current_exe()?.with_file_name(format!("{name}{suffix}"));
    ensure!(
        bundled.is_file(),
        "В поставке отсутствует {name}. Подготовьте бинарники ноды."
    );
    Ok(bundled)
}

fn prepare_child(command: &mut Command) {
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(not(any(windows, unix)))]
    let _ = command;
}

fn full_command(settings: &Settings, binary: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .arg("native-sync")
        .arg("--db")
        .arg(settings.data_dir.join("state"))
        .arg("--p2p-listen")
        .arg(format!("127.0.0.1:{}", settings.p2p_port))
        .arg("--api-listen")
        .arg(format!("127.0.0.1:{}", settings.api_port))
        .arg("--shutdown-on-stdin-close");
    match settings.storage_profile {
        StorageProfile::Full => {
            command.args(["--storage-profile", "full"]);
        }
        StorageProfile::Archive => {
            command.args(["--storage-profile", "archive"]);
        }
        StorageProfile::Custom => {
            command
                .arg("--keep-iavl-versions")
                .arg(settings.keep_versions.to_string())
                .arg("--keep-event-blocks")
                .arg(settings.keep_event_blocks.to_string())
                .arg("--keep-native-blocks")
                .arg(settings.keep_native_blocks.to_string());
        }
    }
    for peer in &settings.peers {
        command.arg("--peer").arg(peer);
    }
    if !settings.peers.is_empty() {
        command.arg("--follow");
    }
    match settings.full_start {
        FullStart::Resume => {}
        FullStart::Genesis => {
            command.arg("--genesis").arg(&settings.genesis);
        }
        FullStart::Checkpoint => {
            command
                .arg("--checkpoint")
                .arg(&settings.checkpoint)
                .arg("--checkpoint-sha256")
                .arg(&settings.checkpoint_sha256);
        }
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    prepare_child(&mut command);
    command
}

async fn run_full(
    settings: &Settings,
    binary: &Path,
    log: Arc<Mutex<Log>>,
    stop: watch::Receiver<bool>,
    full_node_id: Arc<Mutex<Option<String>>>,
) -> Result<()> {
    let mut child = full_command(settings, binary)
        .spawn()
        .context("Не удалось запустить minterd")?;
    // Tokio's Child::wait closes child.stdin before waiting. Keep this parent
    // lifetime channel outside Child until the user requests shutdown.
    let parent_input = child.stdin.take();
    let stdout = tokio::spawn(forward(
        child.stdout.take().context("stdout minterd")?,
        log.clone(),
        Some(full_node_id),
    ));
    let stderr = tokio::spawn(forward(
        child.stderr.take().context("stderr minterd")?,
        log,
        None,
    ));
    let exit = wait_child(&mut child, parent_input, stop).await;
    stdout.await??;
    stderr.await??;
    let exit = exit?;
    ensure!(
        exit.success(),
        "minterd завершился с кодом {exit}. Подробности в журнале."
    );
    Ok(())
}

async fn wait_child(
    child: &mut tokio::process::Child,
    mut parent_input: Option<tokio::process::ChildStdin>,
    mut stop: watch::Receiver<bool>,
) -> Result<std::process::ExitStatus> {
    let requested = async {
        if !*stop.borrow() {
            let _ = stop.changed().await;
        }
    };
    tokio::select! {
        exit = child.wait() => Ok(exit?),
        _ = requested => {
            drop(parent_input.take());
            // Full nodes receive EOF. A one-shot light operation finishes its
            // current redb transaction before the supervisor exits.
            match tokio::time::timeout(Duration::from_secs(40), child.wait()).await {
                Ok(exit) => Ok(exit?),
                Err(_) => {
                    child.kill().await?;
                    bail!("Нода превысила время штатной остановки и была завершена принудительно. Журнал сохранён.");
                }
            }
        }
    }
}

async fn run_light(
    settings: &Settings,
    binary: &Path,
    log: Arc<Mutex<Log>>,
    mut stop: watch::Receiver<bool>,
    state: Arc<Mutex<Status>>,
) -> Result<()> {
    loop {
        if *stop.borrow() {
            return light_stop_result(&state);
        }
        let database = settings.data_dir.join("light.redb");
        let initialize = !database.is_file();
        let mut command = Command::new(binary);
        command
            .args(["light", if initialize { "init" } else { "sync" }])
            .arg("--db")
            .arg(&database)
            .arg("--primary")
            .arg(&settings.primary)
            .arg("--witness")
            .arg(&settings.witness);
        if initialize {
            command.arg("--anchor").arg(&settings.light_anchor);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        prepare_child(&mut command);
        let mut child = command.spawn().context("Не удалось запустить minterctl")?;
        let output = child.stdout.take().context("stdout minterctl")?;
        let stdout = tokio::spawn(async move {
            let mut bytes = Vec::new();
            output.take(64 * 1024 + 1).read_to_end(&mut bytes).await?;
            ensure!(bytes.len() <= 64 * 1024, "Ответ minterctl слишком велик");
            Ok::<_, anyhow::Error>(bytes)
        });
        let stderr = tokio::spawn(forward(
            child.stderr.take().context("stderr minterctl")?,
            log.clone(),
            None,
        ));
        let exit = wait_child(&mut child, None, stop.clone()).await;
        let output = stdout.await??;
        stderr.await??;
        let exit = exit?;
        if *stop.borrow() && !exit.success() {
            bail!("minterctl завершился с кодом {exit} во время остановки. Подробности в журнале.");
        }
        if exit.success() {
            let value: serde_json::Value = serde_json::from_slice(&output)?;
            let node = light_status(&value)?;
            log.lock()
                .unwrap()
                .append(format!("Проверен заголовок {}\n", node.height).as_bytes())?;
            let mut status = state.lock().unwrap();
            status.phase = "running".into();
            status.error = None;
            status.node = Some(node);
        } else {
            state.lock().unwrap().error = Some(format!(
                "Синхронизация заголовков завершилась с кодом {exit}. Подробности в журнале."
            ));
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(10)) => {}
            _ = stop.changed() => return light_stop_result(&state),
        }
    }
}

fn light_stop_result(state: &Mutex<Status>) -> Result<()> {
    let status = state.lock().map_err(|_| anyhow::anyhow!("Статус занят"))?;
    if let Some(error) = &status.error {
        bail!("Последняя синхронизация заголовков завершилась ошибкой: {error}");
    }
    Ok(())
}

fn light_status(value: &serde_json::Value) -> Result<NodeStatus> {
    Ok(NodeStatus {
        network: value["chainId"]
            .as_str()
            .context("Клиент не вернул имя сети")?
            .into(),
        height: json_number(&value["height"])?,
        app_hash: value["appHash"].as_str().unwrap_or_default().into(),
        block_time: value["time"].as_str().unwrap_or_default().into(),
        version: env!("CARGO_PKG_VERSION").into(),
        stale: value["stale"]
            .as_bool()
            .context("Клиент не вернул срок доверия")?,
        catching_up: false,
        trust_expires_at: Some(
            value["trustExpiresAt"]
                .as_str()
                .context("Клиент не вернул срок доверия")?
                .into(),
        ),
    })
}

fn json_number(value: &serde_json::Value) -> Result<String> {
    if let Some(value) = value.as_u64() {
        return Ok(value.to_string());
    }
    let value = value.as_str().context("API не вернул высоту")?;
    value.parse::<u64>().context("API вернул неверную высоту")?;
    Ok(value.into())
}

async fn read_full_status(
    client: &reqwest::Client,
    port: u16,
    expected_id: Option<&str>,
) -> Result<NodeStatus> {
    const MAX_BYTES: usize = 64 * 1024;
    let mut response = client
        .get(format!("http://127.0.0.1:{port}/v2/status"))
        .send()
        .await?
        .error_for_status()?;
    ensure!(
        response
            .content_length()
            .is_none_or(|n| n <= MAX_BYTES as u64),
        "Ответ локального API превышает 64 КиБ"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            chunk.len() <= MAX_BYTES - bytes.len(),
            "Ответ локального API превышает 64 КиБ"
        );
        bytes.extend_from_slice(&chunk);
    }
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).context("Неверный ответ локального API")?;
    if let Some(expected) = expected_id {
        ensure!(
            value["node_id"]
                .as_str()
                .is_some_and(|id| id.eq_ignore_ascii_case(expected)),
            "Локальный API принадлежит другой ноде"
        );
    }
    full_status(&value)
}

fn full_status(value: &serde_json::Value) -> Result<NodeStatus> {
    Ok(NodeStatus {
        network: value["network"]
            .as_str()
            .context("API не вернул имя сети")?
            .into(),
        height: json_number(&value["latest_block_height"])?,
        app_hash: value["latest_app_hash"].as_str().unwrap_or_default().into(),
        block_time: value["latest_block_time"]
            .as_str()
            .unwrap_or_default()
            .into(),
        version: value["version"].as_str().unwrap_or_default().into(),
        catching_up: value["catching_up"].as_bool().unwrap_or(true),
        stale: false,
        trust_expires_at: None,
    })
}

struct Log {
    path: PathBuf,
    file: Option<File>,
    size: u64,
}

impl Log {
    fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            path: path.into(),
            file: Some(file),
            size,
        })
    }

    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        if self.size + bytes.len() as u64 > LOG_LIMIT {
            self.file.take();
            let previous = self.path.with_extension("log.1");
            if previous.exists() {
                fs::remove_file(&previous)?;
            }
            fs::rename(&self.path, previous)?;
            self.file = Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)?,
            );
            self.size = 0;
        }
        self.file
            .as_mut()
            .context("Журнал закрыт")?
            .write_all(bytes)?;
        self.size += bytes.len() as u64;
        Ok(())
    }
}

async fn forward(
    mut source: impl AsyncRead + Unpin,
    log: Arc<Mutex<Log>>,
    mut full_node_id: Option<Arc<Mutex<Option<String>>>>,
) -> Result<()> {
    let mut bytes = [0; 8192];
    // Observe only this child's stdout, keeping a bounded prefix across reads.
    // The startup line is emitted after snapshot/history and HTTP/P2P binding.
    let mut prefix = Vec::with_capacity(128);
    loop {
        let count = source.read(&mut bytes).await?;
        if count == 0 {
            return Ok(());
        }
        log.lock()
            .map_err(|_| anyhow::anyhow!("Журнал занят"))?
            .append(&bytes[..count])?;
        if full_node_id.is_some() {
            for byte in &bytes[..count] {
                if *byte == b'\n' {
                    if let Some(id) = std::str::from_utf8(&prefix)
                        .ok()
                        .and_then(|line| line.strip_prefix("Tendermint 0.34 P2P listening on "))
                        .and_then(|line| line.split_once('@'))
                        .map(|(id, _)| id)
                        .filter(|id| id.len() == 40 && id.bytes().all(|b| b.is_ascii_hexdigit()))
                    {
                        *full_node_id
                            .take()
                            .expect("observing child startup")
                            .lock()
                            .map_err(|_| anyhow::anyhow!("Статус занят"))? =
                            Some(id.to_ascii_lowercase());
                        break;
                    }
                    prefix.clear();
                } else if prefix.len() < 128 {
                    prefix.push(*byte);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_inflight_http_status_cannot_hide_worker_completion() -> Result<()> {
        use tokio::{io::AsyncWriteExt, net::TcpListener};
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let directory = tempfile::tempdir()?;
        let config = directory.path().join("settings.json");
        let mut settings = Settings::defaults(directory.path().join("node"));
        settings.mode = Mode::Full;
        settings.api_port = listener.local_addr()?.port();
        settings.save(&config)?;
        let controller = Controller::new(config, settings.data_dir)?;
        *controller.full_node_id.lock().unwrap() = Some("a".repeat(40));
        *controller.state.lock().unwrap() = Status {
            phase: "starting".into(),
            managed: true,
            ..Default::default()
        };
        let state = controller.state.clone();
        let api = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut input = [0; 8192];
            ensure!(
                stream.read(&mut input).await? > 0,
                "missing test HTTP request"
            );
            *state.lock().unwrap() = Status {
                phase: "error".into(),
                error: Some("worker failed during HTTP request".into()),
                ..Default::default()
            };
            let body = serde_json::to_vec(&serde_json::json!({
                "node_id": "a".repeat(40), "network": "test-chain",
                "latest_block_height": "900", "catching_up": false,
            }))?;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await?;
            stream.write_all(&body).await?;
            Ok::<_, anyhow::Error>(())
        });
        let status = controller.status().await?;
        api.await??;
        assert_eq!(status.phase, "error");
        assert!(!status.managed && !status.controllable && status.node.is_none());
        assert_eq!(
            status.error.as_deref(),
            Some("worker failed during HTTP request")
        );
        Ok(())
    }

    #[tokio::test]
    async fn managed_api_validates_owned_identity_and_response_size() -> Result<()> {
        use tokio::{io::AsyncWriteExt, net::TcpListener};
        for (managed, id, padding, chunked, accepted) in [
            (true, "b".repeat(40), 0, false, false),
            (true, "a".repeat(40), 65536, false, false),
            (true, "a".repeat(40), 65536, true, false),
            (true, "A".repeat(40), 0, false, true),
            (false, "b".repeat(40), 0, false, true),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let directory = tempfile::tempdir()?;
            let config = directory.path().join("settings.json");
            let mut settings = Settings::defaults(directory.path().join("node"));
            settings.mode = Mode::Full;
            settings.api_port = listener.local_addr()?.port();
            settings.save(&config)?;
            let body = serde_json::to_vec(&serde_json::json!({
                "node_id": id, "network": "test-chain", "latest_block_height": "900",
                "latest_app_hash": "aabb", "catching_up": false, "padding": "x".repeat(padding),
            }))?;
            let api = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await?;
                let mut request = [0; 8192];
                ensure!(
                    stream.read(&mut request).await? > 0,
                    "missing test HTTP request"
                );
                let length = if chunked {
                    "Transfer-Encoding: chunked".into()
                } else {
                    format!("Content-Length: {}", body.len())
                };
                stream
                    .write_all(
                        format!("HTTP/1.1 200 OK\r\n{length}\r\nConnection: close\r\n\r\n")
                            .as_bytes(),
                    )
                    .await?;
                // Early close on a rejected length is expected.
                if chunked {
                    let mut framed = format!("{:x}\r\n", body.len()).into_bytes();
                    framed.extend(body);
                    framed.extend(b"\r\n0\r\n\r\n");
                    let _ = stream.write_all(&framed).await;
                } else {
                    let _ = stream.write_all(&body).await;
                }
                Ok::<_, anyhow::Error>(())
            });
            let controller = Controller::new(config, settings.data_dir)?;
            *controller.full_node_id.lock().unwrap() = Some("a".repeat(40));
            *controller.state.lock().unwrap() = Status {
                phase: if managed { "starting" } else { "stopped" }.into(),
                managed,
                control_id: (!managed).then(|| "retired controller".into()),
                ..Default::default()
            };
            let status = controller.status().await?;
            api.await??;
            assert_eq!(
                status.node.is_some(),
                accepted,
                "managed={managed}, padding={padding}, chunked={chunked}"
            );
            assert_eq!(status.controllable, managed);
            if !managed {
                assert!(status.control_id.is_none());
            }
            if accepted {
                assert_eq!(status.phase, if managed { "running" } else { "attached" });
                assert!(status.error.is_none());
            } else {
                assert!(status.error.is_some());
                assert_eq!(status.phase, "starting");
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn stdout_readiness_survives_split_and_long_log_lines() -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("node.log");
        let log = Arc::new(Mutex::new(Log::open(&path)?));
        let id = "a".repeat(40);
        let mut bytes = vec![b'x'; 20000];
        bytes.extend(format!("Tendermint 0.34 P2P listening on {id}@127.0.0.1:1\n").as_bytes());
        bytes.extend(b"Tendermint 0.34 P2P listening on invalid@127.0.0.1:1\n");
        bytes.extend(
            format!(
                "Tendermint 0.34 P2P listening on {}@127.0.0.1:1\n",
                id.to_uppercase()
            )
            .as_bytes(),
        );
        bytes.extend(
            format!(
                "Tendermint 0.34 P2P listening on {}@127.0.0.1:2\n",
                "b".repeat(40)
            )
            .as_bytes(),
        );
        let expected_log = bytes.clone();
        let (mut input, output) = tokio::io::duplex(17);
        let writer = tokio::spawn(async move {
            for chunk in bytes.chunks(7) {
                input.write_all(chunk).await?;
            }
            input.shutdown().await
        });
        let ready = Arc::new(Mutex::new(None));
        forward(output, log, Some(ready.clone())).await?;
        writer.await??;
        assert_eq!(ready.lock().unwrap().as_deref(), Some(id.as_str()));
        assert_eq!(fs::read(path)?, expected_log);
        Ok(())
    }

    #[tokio::test]
    async fn managed_bootstrap_does_not_observe_another_nodes_api() -> Result<()> {
        use tokio::{io::AsyncWriteExt, net::TcpListener};
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let directory = tempfile::tempdir()?;
        let config = directory.path().join("settings.json");
        let mut settings = Settings::defaults(directory.path().join("node"));
        settings.mode = Mode::Full;
        settings.api_port = listener.local_addr()?.port();
        settings.save(&config)?;
        let other_node = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut input = [0; 8192];
            ensure!(
                stream.read(&mut input).await? > 0,
                "missing test HTTP request"
            );
            let body = r#"{"network":"another-chain","latest_block_height":"900","latest_app_hash":"aabb","catching_up":false}"#;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await?;
            Ok::<_, anyhow::Error>(())
        });
        let controller = Controller::new(config, settings.data_dir)?;
        *controller.state.lock().unwrap() = Status {
            phase: "starting".into(),
            managed: true,
            ..Default::default()
        };
        let result = controller.status().await;
        other_node.abort();
        let status = result?;
        assert_eq!(status.phase, "starting");
        assert!(status.managed && status.controllable && status.node.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn a_failed_local_run_does_not_hide_a_new_authenticated_owner() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let config = directory.path().join("settings.json");
        let mut settings = Settings::defaults(directory.path().to_path_buf());
        settings.mode = Mode::Full;
        settings.save(&config)?;
        let controller = Controller::new(config, settings.data_dir.clone())?;
        *controller.state.lock().unwrap() = Status {
            phase: "error".into(),
            error: Some("old local worker failure".into()),
            ..Default::default()
        };
        let server = crate::ipc::Server::bind(&settings.data_dir, Mode::Full).await?;
        let id = server.id().to_owned();
        let (stop, mut stopped) = watch::channel(false);
        let task = tokio::spawn(async move {
            let (result, pending) = server
                .serve_until(
                    async move {
                        stopped.changed().await?;
                        Ok(())
                    },
                    || async {
                        Ok(Status {
                            phase: "running".into(),
                            managed: true,
                            ..Default::default()
                        })
                    },
                    &stop,
                )
                .await;
            if let Some(pending) = pending {
                pending.finish(&result).await?;
            }
            result
        });
        let status = controller.status().await?;
        assert!(status.controllable && !status.managed);
        assert_eq!(status.control_id.as_deref(), Some(id.as_str()));
        assert_eq!(
            status.previous_error.as_deref(),
            Some("old local worker failure")
        );
        assert!(status.error.is_none());
        assert!(controller.stop_owned().await.is_err());
        assert!(!task.is_finished());
        controller.stop_target(Some(&id)).await?;
        task.await??;
        Ok(())
    }

    #[tokio::test]
    async fn light_observation_never_opens_redb_while_a_data_owner_holds_the_lease() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let data = directory.path().join("node");
        fs::create_dir(&data)?;
        fs::write(data.join("light.redb"), b"must not be opened")?;
        let config = directory.path().join("settings.json");
        Settings::defaults(data.clone()).save(&config)?;
        let controller = Controller::new(config, data.clone())?;
        let _lease = exclusive_lock(&data.join("controller.lock"))?;
        let status = controller.status().await?;
        assert!(!status.managed && !status.controllable && status.node.is_none());
        assert!(
            status
                .error
                .as_deref()
                .is_some_and(|message| message.contains("Папка данных занята"))
        );
        assert_eq!(fs::read(data.join("light.redb"))?, b"must not be opened");
        Ok(())
    }

    #[tokio::test]
    async fn owner_cleanup_never_stops_a_replacement_controller() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let config = directory.path().join("settings.json");
        let mut settings = Settings::defaults(directory.path().to_path_buf());
        settings.mode = Mode::Full;
        settings.save(&config)?;
        let old = Controller::new(config, settings.data_dir.clone())?;
        let server = crate::ipc::Server::bind(&settings.data_dir, Mode::Full).await?;
        let id = server.id().to_owned();
        let (stop, mut stopped) = watch::channel(false);
        let replacement = tokio::spawn(async move {
            let (result, pending) = server
                .serve_until(
                    async move {
                        stopped.changed().await?;
                        Ok(())
                    },
                    || async { Ok(Status::default()) },
                    &stop,
                )
                .await;
            if let Some(pending) = pending {
                pending.finish(&result).await?;
            }
            result
        });
        old.stop_owned().await?;
        assert!(!replacement.is_finished());
        old.stop_target(Some(&id)).await?;
        replacement.await??;
        Ok(())
    }

    #[tokio::test]
    async fn terminal_worker_error_cannot_be_replaced_by_another_nodes_api() -> Result<()> {
        use tokio::{io::AsyncWriteExt, net::TcpListener};
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let other_node = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut input = [0; 8192];
            ensure!(
                stream.read(&mut input).await? > 0,
                "missing test HTTP request"
            );
            let body = r#"{"network":"another-chain","latest_block_height":"900","latest_app_hash":"aabb","catching_up":false}"#;
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await?;
            Ok::<_, anyhow::Error>(())
        });
        let directory = tempfile::tempdir()?;
        let config = directory.path().join("settings.json");
        let mut settings = Settings::defaults(directory.path().join("node"));
        settings.mode = Mode::Full;
        settings.api_port = port;
        settings.save(&config)?;
        let controller = Controller::new(config, settings.data_dir)?;
        *controller.state.lock().unwrap() = Status {
            phase: "error".into(),
            error: Some("original child bind failure".into()),
            ..Default::default()
        };
        let result = controller.status().await;
        other_node.abort();
        let status = result?;
        assert_eq!(status.phase, "error");
        assert_eq!(status.error.as_deref(), Some("original child bind failure"));
        assert!(status.node.is_none());
        Ok(())
    }

    #[test]
    fn cached_light_header_expires_at_its_deadline() -> Result<()> {
        let deadline = time::OffsetDateTime::parse(
            "2026-11-13T18:00:00Z",
            &time::format_description::well_known::Rfc3339,
        )?;
        let mut node = NodeStatus {
            trust_expires_at: Some("2026-11-13T18:00:00Z".into()),
            ..Default::default()
        };
        node.refresh_expiration(deadline - time::Duration::NANOSECOND)?;
        assert!(!node.stale);
        node.refresh_expiration(deadline)?;
        assert!(node.stale);
        Ok(())
    }

    #[test]
    fn full_node_arguments_preserve_paths_and_keep_api_local() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut settings = Settings::defaults(dir.path().join("Нода Minter"));
        settings.mode = Mode::Full;
        settings.full_start = FullStart::Genesis;
        settings.genesis = dir.path().join("Моя сеть/genesis.json");
        let command = full_command(&settings, Path::new("minterd"));
        let args = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            args.windows(2)
                .any(|a| a == ["--p2p-listen", "127.0.0.1:26656"])
        );
        assert!(
            args.windows(2)
                .any(|a| a[0] == "--genesis" && a[1] == settings.genesis.to_string_lossy())
        );
        assert!(args.contains(&"--shutdown-on-stdin-close".into()));
        assert!(args.windows(2).any(|a| a == ["--storage-profile", "full"]));
        assert!(!args.contains(&"--keep-iavl-versions".into()));
        assert!(!args.iter().any(|a| a.starts_with("--validator")));
        settings.storage_profile = StorageProfile::Archive;
        let command = full_command(&settings, Path::new("minterd"));
        let args = command.as_std().get_args().collect::<Vec<_>>();
        assert!(
            args.windows(2)
                .any(|a| a == ["--storage-profile", "archive"])
        );
        settings.storage_profile = StorageProfile::Custom;
        settings.keep_versions = 17;
        settings.keep_event_blocks = 0;
        settings.keep_native_blocks = 0;
        let command = full_command(&settings, Path::new("minterd"));
        let args = command.as_std().get_args().collect::<Vec<_>>();
        for pair in [
            ["--keep-iavl-versions", "17"],
            ["--keep-event-blocks", "0"],
            ["--keep-native-blocks", "0"],
        ] {
            assert!(args.windows(2).any(|a| a == pair));
        }
        assert!(!args.contains(&std::ffi::OsStr::new("--storage-profile")));
        Ok(())
    }

    #[tokio::test]
    async fn configuration_lease_blocks_other_writers_and_stop_reports_failure() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let config = dir.path().join("settings.json");
        let controller = Controller::new(config.clone(), dir.path().join("node"))?;
        let lease = configuration_lock(&config)?;
        assert!(controller.save(controller.settings()?).await.is_err());
        #[cfg(unix)]
        {
            controller.settings()?.save_new(&config)?;
            let alias = dir.path().join("alias.json");
            std::os::unix::fs::symlink(&config, &alias)?;
            assert!(configuration_lock(&alias).is_err());
            assert_eq!(
                Controller::new(alias, dir.path().join("other"))?.config,
                config.canonicalize()?
            );
        }
        drop(lease);
        controller.save(controller.settings()?).await?;
        let (stop, _) = watch::channel(false);
        *controller.state.lock().unwrap() = Status {
            phase: "error".into(),
            error: Some("child failed".into()),
            ..Default::default()
        };
        *controller.active.lock().await = Some(Active {
            stop,
            task: tokio::spawn(async {}),
        });
        assert!(
            controller
                .stop()
                .await
                .unwrap_err()
                .to_string()
                .contains("child failed")
        );
        Ok(())
    }

    #[test]
    fn parses_wire_status_and_rotates_bounded_log() -> Result<()> {
        let status = full_status(&serde_json::json!({
            "network": "minter-mainnet-5", "latest_block_height": "40700741",
            "catching_up": false
        }))?;
        assert_eq!(status.height, "40700741");
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("node.log");
        let mut log = Log::open(&path)?;
        log.append(&vec![b'x'; LOG_LIMIT as usize])?;
        log.append(b"next\n")?;
        assert_eq!(fs::read(&path)?, b"next\n");
        assert_eq!(fs::metadata(path.with_extension("log.1"))?.len(), LOG_LIMIT);
        Ok(())
    }
}
