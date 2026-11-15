use crate::{Mode, Status};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{future::Future, path::Path, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::watch,
    time::timeout,
};

#[cfg(unix)]
#[path = "ipc/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "ipc/windows.rs"]
mod platform;

const VERSION: u8 = 1;
const REQUEST_LIMIT: usize = 1024;
const RESPONSE_LIMIT: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum Command {
    Status,
    Stop,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    version: u8,
    mode: Mode,
    command: Command,
    instance: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub(super) enum Reply {
    Status(Box<Status>),
    Stopped,
    Error { message: String },
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Response {
    version: u8,
    mode: Mode,
    instance: String,
    reply: Reply,
}

pub(super) struct Server {
    listener: platform::Listener,
    mode: Mode,
    id: String,
}

impl Server {
    pub(super) async fn bind(directory: &Path, mode: Mode) -> Result<Self> {
        let mut id = [0; 16];
        getrandom::fill(&mut id)
            .map_err(|error| anyhow::anyhow!("Создание ID контроллера: {error}"))?;
        Ok(Self {
            listener: platform::Listener::bind(directory).await?,
            mode,
            id: format!("{:032x}", u128::from_le_bytes(id)),
        })
    }

    pub(super) fn id(&self) -> &str {
        &self.id
    }

    pub(super) async fn serve_until<R, O, F>(
        mut self,
        running: R,
        observe: O,
        stop: &watch::Sender<bool>,
    ) -> (Result<()>, Option<PendingStop>)
    where
        R: Future<Output = Result<()>>,
        O: Fn() -> F,
        F: Future<Output = Result<Status>>,
    {
        tokio::pin!(running);
        loop {
            let mut stream = tokio::select! {
                biased;
                result = &mut running => return (result, None),
                accepted = self.listener.accept() => match accepted {
                    Ok(stream) => stream,
                    Err(error) => {
                        stop.send_replace(true);
                        let result = running.await;
                        return (result.and(Err(error).context("Локальный канал управления")), None);
                    }
                },
            };
            let request = async {
                platform::authenticate_client(&stream)?;
                let request: Request = read_message(&mut stream, REQUEST_LIMIT).await?;
                ensure!(
                    request.version == VERSION,
                    "Неподдерживаемая версия управления"
                );
                ensure!(
                    request.mode == self.mode,
                    "Папка данных используется другим режимом ноды"
                );
                if matches!(request.command, Command::Stop) {
                    ensure!(
                        request.instance.as_deref() == Some(self.id()),
                        "Владелец ноды изменился; обновите статус"
                    );
                }
                Ok::<_, anyhow::Error>(request)
            };
            let received = tokio::select! {
                biased;
                result = &mut running => return (result, None),
                request = timeout(IO_TIMEOUT, request) => request,
            };
            let request = match received {
                Ok(Ok(request)) => request,
                result => {
                    let message = match result {
                        Ok(Err(error)) => format!("{error:#}"),
                        _ => "Истекло время чтения команды управления".into(),
                    };
                    let _ =
                        respond(&mut stream, self.mode, &self.id, Reply::Error { message }).await;
                    continue;
                }
            };
            if matches!(request.command, Command::Stop) {
                stop.send_replace(true);
                let result = running.await;
                return (
                    result,
                    Some(PendingStop {
                        stream,
                        mode: self.mode,
                        id: self.id.clone(),
                    }),
                );
            }
            let observed = tokio::select! {
                biased;
                result = &mut running => return (result, None),
                observed = observe() => observed,
            };
            let reply = match observed {
                Ok(mut status) => {
                    status.controllable = true;
                    status.control_id = Some(self.id.clone());
                    Reply::Status(Box::new(status))
                }
                Err(error) => Reply::Error {
                    message: format!("{error:#}"),
                },
            };
            tokio::select! {
                biased;
                result = &mut running => return (result, None),
                _ = respond(&mut stream, self.mode, &self.id, reply) => {}
            }
        }
    }
}

pub(super) struct PendingStop {
    stream: platform::ServerStream,
    mode: Mode,
    id: String,
}

impl PendingStop {
    // Called only after the child is reaped, listener dropped and leases released.
    pub(super) async fn finish(mut self, result: &Result<()>) -> Result<()> {
        let reply = match result {
            Ok(()) => Reply::Stopped,
            Err(error) => Reply::Error {
                message: format!("{error:#}"),
            },
        };
        respond(&mut self.stream, self.mode, &self.id, reply).await
    }
}

pub(super) async fn request(
    directory: &Path,
    mode: Mode,
    command: Command,
    instance: Option<&str>,
) -> Result<Option<Reply>> {
    let limit = if matches!(command, Command::Stop) {
        50
    } else {
        8
    };
    timeout(Duration::from_secs(limit), async {
        let Some(mut stream) = platform::connect(directory).await? else {
            return Ok(None);
        };
        platform::authenticate_server(&stream)?;
        let request = Request {
            version: VERSION,
            mode,
            command,
            instance: instance.map(str::to_owned),
        };
        write_message(&mut stream, &request).await?;
        let response: Response = read_message(&mut stream, RESPONSE_LIMIT).await?;
        ensure!(
            stream.read(&mut [0]).await? == 0,
            "Лишние данные локального управления"
        );
        ensure!(
            response.version == VERSION,
            "Неподдерживаемая версия ответа управления"
        );
        if let Reply::Error { message } = response.reply {
            bail!("{message}");
        }
        ensure!(response.mode == mode, "Неверный режим ответа управления");
        if let Some(instance) = instance {
            ensure!(
                response.instance == instance,
                "Владелец ноды изменился; обновите статус"
            );
        }
        if let Reply::Status(status) = &response.reply {
            ensure!(
                status.control_id.as_deref() == Some(response.instance.as_str()),
                "Неверный ID статуса управления"
            );
        }
        Ok(Some(response.reply))
    })
    .await
    .context("Истекло время локального управления нодой")?
}

async fn read_message<T: DeserializeOwned>(
    stream: &mut (impl AsyncRead + Unpin),
    limit: usize,
) -> Result<T> {
    let length = stream.read_u32().await? as usize;
    ensure!(
        length > 0 && length <= limit,
        "Сообщение управления превышает допустимый размер"
    );
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn write_message(
    stream: &mut (impl AsyncWrite + Unpin),
    value: &impl Serialize,
) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= RESPONSE_LIMIT,
        "Ответ управления превышает допустимый размер"
    );
    stream.write_u32(bytes.len().try_into()?).await?;
    stream.write_all(&bytes).await?;
    Ok(())
}

async fn respond(
    stream: &mut (impl AsyncWrite + Unpin),
    mode: Mode,
    id: &str,
    reply: Reply,
) -> Result<()> {
    timeout(
        IO_TIMEOUT,
        write_message(
            stream,
            &Response {
                version: VERSION,
                mode,
                instance: id.into(),
                reply,
            },
        ),
    )
    .await
    .context("Истекло время ответа управления")?
}

#[cfg(test)]
mod tests;
