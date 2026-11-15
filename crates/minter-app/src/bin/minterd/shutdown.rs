use anyhow::{Context, Result};
use minter_state::{OperationCancelled, OperationControl};
use std::{
    future::{Future, poll_fn},
    io,
    task::Poll,
};
use tokio::{sync::oneshot, task::JoinSet};

pub(super) async fn start(
    on_stdin_close: bool,
    control: &OperationControl,
) -> Result<JoinSet<io::Result<()>>> {
    watch(signal(on_stdin_close), control.clone()).await
}

async fn watch(
    signal: impl Future<Output = io::Result<()>> + Send + 'static,
    control: OperationControl,
) -> Result<JoinSet<io::Result<()>>> {
    let (ready, registered) = oneshot::channel();
    let mut tasks = JoinSet::new();
    tasks.spawn(async move {
        tokio::pin!(signal);
        // ctrl_c registers its handler only on first poll. Acknowledge that poll
        // before the caller starts any blocking bootstrap work.
        let first = poll_fn(|cx| {
            Poll::Ready(match signal.as_mut().poll(cx) {
                Poll::Ready(result) => Some(result),
                Poll::Pending => None,
            })
        })
        .await;
        if first.is_some() {
            control.cancel();
        }
        let _ = ready.send(());
        let result = match first {
            Some(result) => result,
            None => signal.await,
        };
        control.cancel();
        result
    });
    registered
        .await
        .context("register native shutdown watcher")?;
    Ok(tasks)
}

pub(super) async fn wait(tasks: &mut JoinSet<io::Result<()>>) -> Result<()> {
    tasks
        .join_next()
        .await
        .context("missing native shutdown watcher")?
        .context("native shutdown watcher")?
        .context("wait for native shutdown")
}

pub(super) async fn finish(
    result: Result<()>,
    control: &OperationControl,
    tasks: &mut JoinSet<io::Result<()>>,
) -> Result<()> {
    let cancelled = result.as_ref().is_err_and(OperationCancelled::caused);
    if control.is_cancelled() && (cancelled || result.is_ok()) {
        // The P2P loop may already have consumed and checked the watcher.
        if !tasks.is_empty() {
            wait(tasks).await?;
        }
        if cancelled {
            eprintln!("Native startup cancelled; database closed");
        }
        return Ok(());
    }
    result
}

pub(super) async fn service_exit(
    service: Option<std::result::Result<Result<()>, tokio::task::JoinError>>,
    control: &OperationControl,
    tasks: &mut JoinSet<io::Result<()>>,
) -> Result<()> {
    match service {
        Some(Ok(Ok(()))) if control.is_cancelled() => wait(tasks).await,
        Some(Ok(Err(error))) => Err(error),
        Some(Err(error)) => Err(error).context("native node service task"),
        _ => Err(anyhow::anyhow!("native node service stopped unexpectedly")),
    }
}

pub(super) async fn drain(tasks: &mut JoinSet<Result<()>>) -> Result<()> {
    let mut result = Ok(());
    while let Some(done) = tasks.join_next().await {
        let done = done.context("native worker task").and_then(|result| result);
        result = result.and(done);
    }
    result
}

async fn signal(on_stdin_close: bool) -> io::Result<()> {
    let signal = async {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut terminate = signal(SignalKind::terminate())?;
            tokio::select! { result = tokio::signal::ctrl_c() => result, _ = terminate.recv() => Ok(()) }
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
        let (send, receive) = oneshot::channel();
        std::thread::Builder::new()
            .name("desktop-parent".into())
            .spawn(move || {
                use std::io::Read;
                let result =
                    io::copy(&mut io::stdin().lock().take(u64::MAX), &mut io::sink()).map(|_| ());
                let _ = send.send(result);
            })?;
        receive.await.map_err(io::Error::other)?
    };
    tokio::select! { result = signal => result, result = stdin => result }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ready_and_late_requests_remain_set_across_bootstrap() -> Result<()> {
        let control = OperationControl::default();
        let mut tasks = watch(async { Ok(()) }, control.clone()).await?;
        assert!(control.is_cancelled());
        wait(&mut tasks).await?;
        assert!(control.check().is_err());
        let control = OperationControl::default();
        let (send, receive) = oneshot::channel();
        let mut tasks = watch(
            async { receive.await.map_err(io::Error::other) },
            control.clone(),
        )
        .await?;
        assert!(!control.is_cancelled());
        send.send(()).unwrap();
        wait(&mut tasks).await?;
        assert!(control.is_cancelled());
        Ok(())
    }

    #[tokio::test]
    async fn watcher_failure_is_not_successful_cancellation() -> Result<()> {
        let control = OperationControl::default();
        let mut tasks = watch(
            async { Err(io::Error::other("registration failed")) },
            control.clone(),
        )
        .await?;
        assert!(control.is_cancelled());
        assert!(wait(&mut tasks).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn service_can_finish_before_shutdown_branch_is_selected() -> Result<()> {
        let control = OperationControl::default();
        let mut tasks = watch(async { Ok(()) }, control.clone()).await?;
        service_exit(Some(Ok(Ok(()))), &control, &mut tasks).await?;
        // The service branch has already checked the watcher. Do not wait twice.
        finish(Ok(()), &control, &mut tasks).await?;
        Ok(())
    }

    #[tokio::test]
    async fn successful_worker_does_not_hide_a_failed_shutdown_watcher() -> Result<()> {
        let control = OperationControl::default();
        let mut tasks = watch(
            async { Err(io::Error::other("late stdin failure")) },
            control.clone(),
        )
        .await?;
        let result = finish(Ok(()), &control, &mut tasks).await;
        assert!(format!("{:#}", result.unwrap_err()).contains("late stdin failure"));
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_does_not_hide_a_service_or_watcher_failure() -> Result<()> {
        let control = OperationControl::default();
        let mut tasks = watch(
            async { Err(io::Error::other("late stdin failure")) },
            control.clone(),
        )
        .await?;
        let result = service_exit(Some(Ok(Ok(()))), &control, &mut tasks).await;
        assert!(format!("{:#}", result.unwrap_err()).contains("late stdin failure"));
        let mut tasks = watch(async { Ok(()) }, control.clone()).await?;
        let result = service_exit(
            Some(Ok(Err(anyhow::anyhow!("real service failure")))),
            &control,
            &mut tasks,
        )
        .await;
        assert!(format!("{:#}", result.unwrap_err()).contains("real service failure"));
        let result = finish(
            Err(anyhow::anyhow!("real import failure")),
            &control,
            &mut tasks,
        )
        .await;
        assert!(format!("{:#}", result.unwrap_err()).contains("real import failure"));
        let control = OperationControl::default();
        let mut tasks = JoinSet::new();
        assert!(
            service_exit(Some(Ok(Ok(()))), &control, &mut tasks)
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn signal_first_still_reports_service_failures_and_joins_other_services() -> Result<()> {
        let control = OperationControl::default();
        let mut signals = watch(async { Ok(()) }, control.clone()).await?;
        let mut services = JoinSet::new();
        services.spawn(async { Err(anyhow::anyhow!("service failed during shutdown")) });
        let (send, receive) = oneshot::channel();
        services.spawn(async move {
            let _ = send.send(());
            Ok(())
        });
        wait(&mut signals).await?;
        let result = drain(&mut services).await;
        let result = finish(result, &control, &mut signals).await;
        assert!(format!("{:#}", result.unwrap_err()).contains("service failed during shutdown"));
        assert!(services.is_empty());
        receive.await?;
        Ok(())
    }
}
