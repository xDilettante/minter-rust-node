use std::{
    fs, io,
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::net::{UnixListener, UnixStream};

pub(super) type ServerStream = UnixStream;

fn uid() -> u32 {
    // SAFETY: geteuid takes no pointers and has no preconditions.
    unsafe { libc::geteuid() }
}

fn address(directory: &Path, create: bool) -> io::Result<Option<PathBuf>> {
    let metadata = match fs::metadata(directory) {
        Ok(metadata) if metadata.is_dir() => metadata,
        Ok(_) => return Err(io::Error::other("Папка данных не является каталогом")),
        Err(error) if error.kind() == io::ErrorKind::NotFound && !create => return Ok(None),
        Err(error) => return Err(error),
    };
    let root = PathBuf::from(format!("/tmp/minter-node-control-{}", uid()));
    if create {
        match fs::DirBuilder::new().mode(0o700).create(&root) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let private = match fs::symlink_metadata(&root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound && !create => return Ok(None),
        Err(error) => return Err(error),
    };
    if !private.is_dir() || private.uid() != uid() || private.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Небезопасный каталог локального управления",
        ));
    }
    Ok(Some(root.join(format!(
        "{:x}-{:x}.sock",
        metadata.dev(),
        metadata.ino()
    ))))
}

pub(super) struct Listener {
    listener: Option<UnixListener>,
    address: PathBuf,
    inode: (u64, u64),
}

impl Listener {
    pub(super) async fn bind(directory: &Path) -> io::Result<Self> {
        let address =
            address(directory, true)?.ok_or_else(|| io::Error::other("Нет папки данных"))?;
        if let Ok(metadata) = fs::symlink_metadata(&address) {
            if !metadata.file_type().is_socket() || metadata.uid() != uid() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Адрес управления занят посторонним файлом",
                ));
            }
            match tokio::time::timeout(Duration::from_millis(100), UnixStream::connect(&address))
                .await
            {
                Ok(Err(error))
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    fs::remove_file(&address)?
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "Локальный контроллер уже слушает адрес",
                    ));
                }
            }
        }
        let socket = UnixListener::bind(&address)?;
        let metadata = fs::symlink_metadata(&address)?;
        let listener = Self {
            listener: Some(socket),
            address,
            inode: (metadata.dev(), metadata.ino()),
        };
        fs::set_permissions(&listener.address, fs::Permissions::from_mode(0o600))?;
        Ok(listener)
    }

    pub(super) async fn accept(&mut self) -> io::Result<UnixStream> {
        Ok(self
            .listener
            .as_ref()
            .expect("live local listener")
            .accept()
            .await?
            .0)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.listener.take();
        if fs::symlink_metadata(&self.address).is_ok_and(|metadata| {
            metadata.file_type().is_socket() && (metadata.dev(), metadata.ino()) == self.inode
        }) {
            let _ = fs::remove_file(&self.address);
        }
    }
}

pub(super) async fn connect(directory: &Path) -> io::Result<Option<UnixStream>> {
    let Some(address) = address(directory, false)? else {
        return Ok(None);
    };
    match UnixStream::connect(address).await {
        Ok(stream) => Ok(Some(stream)),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub(super) fn authenticate_client(stream: &UnixStream) -> io::Result<()> {
    if stream.peer_cred()?.uid() != uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Другой пользователь локального канала",
        ));
    }
    Ok(())
}

pub(super) use authenticate_client as authenticate_server;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn permissions_aliases_and_stale_socket_cleanup() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let aliases = tempfile::tempdir()?;
        let alias = aliases.path().join("alias");
        std::os::unix::fs::symlink(directory.path(), &alias)?;
        let listener = Listener::bind(directory.path()).await?;
        let path = address(directory.path(), false)?.unwrap();
        assert_eq!(address(&alias, false)?, Some(path.clone()));
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(path.parent().unwrap())?.permissions().mode() & 0o777,
            0o700
        );
        assert!(Listener::bind(directory.path()).await.is_err());
        drop(listener);
        assert!(!path.exists());
        let stale = std::os::unix::net::UnixListener::bind(&path)?;
        drop(stale);
        let listener = Listener::bind(directory.path()).await?;
        fs::remove_file(&path)?;
        fs::write(&path, b"unrelated replacement")?;
        drop(listener);
        assert_eq!(fs::read(&path)?, b"unrelated replacement");
        assert!(Listener::bind(directory.path()).await.is_err());
        fs::remove_file(path)?;
        Ok(())
    }
}
