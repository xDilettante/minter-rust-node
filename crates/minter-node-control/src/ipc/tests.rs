use super::*;
use crate::{Mode, Status};
use anyhow::Result;
use tokio::sync::{oneshot, watch};

#[tokio::test]
async fn status_stop_and_generation_use_the_real_local_transport() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let server = Server::bind(directory.path(), Mode::Full).await?;
    let id = server.id().to_owned();
    let (stop, mut stopped) = watch::channel(false);
    let (released, after_release) = oneshot::channel();
    let task = tokio::spawn(async move {
        let runner = async move {
            if !*stopped.borrow() {
                stopped.changed().await?;
            }
            Ok(())
        };
        let (result, pending) = server
            .serve_until(runner, || async { Ok(Status::default()) }, &stop)
            .await;
        let _ = released.send(());
        if let Some(pending) = pending {
            pending.finish(&result).await?;
        }
        result
    });
    let Reply::Status(status) = request(directory.path(), Mode::Full, Command::Status, None)
        .await?
        .expect("missing local server")
    else {
        panic!("expected status")
    };
    assert_eq!(status.control_id.as_deref(), Some(id.as_str()));
    assert!(
        request(directory.path(), Mode::Light, Command::Stop, Some(&id))
            .await
            .is_err()
    );
    assert!(
        request(
            directory.path(),
            Mode::Full,
            Command::Stop,
            Some("another-generation")
        )
        .await
        .is_err()
    );
    assert!(!task.is_finished());
    assert!(matches!(
        request(directory.path(), Mode::Full, Command::Stop, Some(&id)).await?,
        Some(Reply::Stopped)
    ));
    after_release.await?;
    task.await??;
    assert!(
        request(directory.path(), Mode::Full, Command::Status, None)
            .await?
            .is_none()
    );
    // A stop bound to the previous generation must not stop its replacement.
    let replacement = Server::bind(directory.path(), Mode::Full).await?;
    assert_ne!(replacement.id(), id);
    let new_id = replacement.id().to_owned();
    let (stop, mut stopped) = watch::channel(false);
    let replacement = tokio::spawn(async move {
        let (result, pending) = replacement
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
    assert!(
        request(directory.path(), Mode::Full, Command::Stop, Some(&id))
            .await
            .is_err()
    );
    assert!(!replacement.is_finished());
    request(directory.path(), Mode::Full, Command::Stop, Some(&new_id)).await?;
    replacement.await??;
    Ok(())
}

#[tokio::test]
async fn invalid_frames_do_not_stop_the_worker_and_its_failure_is_returned() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let server = Server::bind(directory.path(), Mode::Full).await?;
    let id = server.id().to_owned();
    let (stop, mut stopped) = watch::channel(false);
    let task = tokio::spawn(async move {
        let (result, pending) = server
            .serve_until(
                async move {
                    stopped.changed().await?;
                    bail!("worker failed while stopping")
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
    for message in [
        serde_json::json!({"version":2,"mode":"full","command":"status"}),
        serde_json::json!({"version":1,"mode":"full","command":"unknown"}),
        serde_json::json!({"version":1,"mode":"full","command":"status","extra":true}),
        serde_json::json!({"version":1,"mode":"full","command":"stop"}),
    ] {
        let mut stream = platform::connect(directory.path())
            .await?
            .expect("local transport");
        write_message(&mut stream, &message).await?;
        let result: Response = read_message(&mut stream, RESPONSE_LIMIT).await?;
        assert!(matches!(result.reply, Reply::Error { .. }));
        assert!(stream.read(&mut [0]).await? == 0);
        assert!(!task.is_finished());
    }
    let mut oversized = platform::connect(directory.path())
        .await?
        .expect("local transport");
    oversized.write_u32(REQUEST_LIMIT as u32 + 1).await?;
    let rejected: Response = read_message(&mut oversized, RESPONSE_LIMIT).await?;
    assert!(matches!(rejected.reply, Reply::Error { .. }));
    assert!(oversized.read(&mut [0]).await? == 0);
    drop(oversized);
    let mut broken = platform::connect(directory.path())
        .await?
        .expect("local transport");
    broken.write_u32(500).await?;
    broken.write_all(b"{").await?;
    drop(broken);
    assert!(matches!(
        request(directory.path(), Mode::Full, Command::Status, None).await?,
        Some(Reply::Status(_))
    ));
    let error = request(directory.path(), Mode::Full, Command::Stop, Some(&id))
        .await
        .err()
        .expect("worker failure");
    assert!(format!("{error:#}").contains("worker failed while stopping"));
    assert!(task.await?.is_err());
    Ok(())
}

#[tokio::test]
async fn missing_transport_does_not_create_a_profile_directory() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let missing = directory.path().join("missing");
    assert!(
        request(&missing, Mode::Full, Command::Status, None)
            .await?
            .is_none()
    );
    assert!(!missing.exists());
    Ok(())
}
