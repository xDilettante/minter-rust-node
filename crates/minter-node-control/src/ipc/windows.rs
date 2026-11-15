use std::{
    fs::OpenOptions,
    io,
    mem::{size_of, zeroed},
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
    ptr::null_mut,
    time::{Duration, Instant},
};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::{
    Foundation::{ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY, HANDLE, LocalFree},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SDDL_REVISION_1,
        },
        GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, GetFileInformationByHandle,
        SECURITY_IDENTIFICATION,
    },
    System::{
        Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId},
        Threading::{
            GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
};

pub(super) type ServerStream = NamedPipeServer;

fn checked(result: i32) -> io::Result<()> {
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

struct LocalMemory(*mut core::ffi::c_void);
impl Drop for LocalMemory {
    fn drop(&mut self) {
        // SAFETY: both conversion APIs allocate this buffer with LocalAlloc.
        unsafe {
            LocalFree(self.0);
        }
    }
}

fn user_sid(process: HANDLE) -> io::Result<String> {
    let mut token = null_mut();
    // SAFETY: process is a valid borrowed handle; token points to writable storage.
    checked(unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) })?;
    // SAFETY: successful OpenProcessToken returned an owned valid handle.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut bytes = 0;
    // SAFETY: null/zero requests the required TokenUser buffer size.
    unsafe {
        GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut bytes);
    }
    if !(size_of::<TOKEN_USER>()..=1024).contains(&(bytes as usize)) {
        return Err(io::Error::other("Неверный размер SID пользователя"));
    }
    // usize provides TOKEN_USER's pointer alignment, unlike a Vec<u8> cast.
    let mut buffer = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
    // SAFETY: the aligned buffer has at least bytes writable bytes.
    checked(unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        )
    })?;
    // SAFETY: successful TokenUser query initialized the aligned TOKEN_USER and SID.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut text = null_mut();
    // SAFETY: SID remains valid in buffer, output points to writable pointer storage.
    checked(unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) })?;
    let allocated = LocalMemory(text.cast());
    let mut length = 0;
    // SAFETY: successful conversion returned a NUL-terminated UTF-16 string.
    let sid = unsafe {
        while *text.add(length) != 0 {
            length += 1;
        }
        String::from_utf16(std::slice::from_raw_parts(text, length)).map_err(io::Error::other)?
    };
    drop(allocated);
    Ok(sid)
}

fn current_sid() -> io::Result<String> {
    // SAFETY: GetCurrentProcess returns a valid non-owned pseudo handle.
    user_sid(unsafe { GetCurrentProcess() })
}

fn address(directory: &Path) -> io::Result<Option<String>> {
    let folder = match OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(directory)
    {
        Ok(folder) => folder,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !folder.metadata()?.is_dir() {
        return Err(io::Error::other("Папка данных не является каталогом"));
    }
    // SAFETY: this SDK structure consists of integer/FILETIME fields, valid at zero.
    let mut identity: BY_HANDLE_FILE_INFORMATION = unsafe { zeroed() };
    // SAFETY: directory handle is valid and identity is writable.
    checked(unsafe { GetFileInformationByHandle(folder.as_raw_handle(), &mut identity) })?;
    let index = u64::from(identity.nFileIndexHigh) << 32 | u64::from(identity.nFileIndexLow);
    Ok(Some(format!(
        r"\\.\pipe\minter-node-{}-{:08x}-{index:016x}",
        current_sid()?,
        identity.dwVolumeSerialNumber
    )))
}

fn create(name: &str, first: bool) -> io::Result<NamedPipeServer> {
    let sid = current_sid()?;
    let descriptor: Vec<u16> = format!("O:{sid}D:P(A;;GA;;;{sid})")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut security = null_mut();
    // SAFETY: input is NUL-terminated; security is initialized by the conversion.
    checked(unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor.as_ptr(),
            SDDL_REVISION_1,
            &mut security,
            null_mut(),
        )
    })?;
    let memory = LocalMemory(security);
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: memory.0,
        bInheritHandle: 0,
    };
    // SAFETY: attributes and its valid descriptor remain alive for the complete call;
    // CreateNamedPipe copies the descriptor rather than retaining these pointers.
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                name,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
    }
}

pub(super) struct Listener {
    next: Option<NamedPipeServer>,
    address: String,
}

impl Listener {
    pub(super) async fn bind(directory: &Path) -> io::Result<Self> {
        let address = address(directory)?.ok_or_else(|| io::Error::other("Нет папки данных"))?;
        let started = Instant::now();
        let next = loop {
            match create(&address, true) {
                Ok(server) => break server,
                Err(error)
                    if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32)
                        && started.elapsed() < Duration::from_secs(1) =>
                {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                Err(error) => return Err(error),
            }
        };
        Ok(Self {
            next: Some(next),
            address,
        })
    }

    pub(super) async fn accept(&mut self) -> io::Result<NamedPipeServer> {
        self.next
            .as_ref()
            .expect("live local listener")
            .connect()
            .await?;
        let next = create(&self.address, false)?;
        Ok(self.next.replace(next).expect("connected local pipe"))
    }
}

pub(super) async fn connect(directory: &Path) -> io::Result<Option<NamedPipeClient>> {
    let Some(address) = address(directory)? else {
        return Ok(None);
    };
    let started = Instant::now();
    loop {
        match ClientOptions::new()
            .security_qos_flags(SECURITY_IDENTIFICATION)
            .open(&address)
        {
            Ok(stream) => return Ok(Some(stream)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error)
                if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                    && started.elapsed() < Duration::from_secs(5) =>
            {
                tokio::time::sleep(Duration::from_millis(25)).await
            }
            Err(error) => return Err(error),
        }
    }
}

fn authenticate(pid: u32) -> io::Result<()> {
    // SAFETY: OpenProcess takes scalar arguments and returns an owned handle.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful OpenProcess returned a valid owned handle.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    if user_sid(process.as_raw_handle())? != current_sid()? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Другой пользователь локального канала",
        ));
    }
    Ok(())
}

pub(super) fn authenticate_client(stream: &NamedPipeServer) -> io::Result<()> {
    let mut pid = 0;
    // SAFETY: stream is a valid server pipe and pid is writable.
    checked(unsafe { GetNamedPipeClientProcessId(stream.as_raw_handle(), &mut pid) })?;
    authenticate(pid)
}

pub(super) fn authenticate_server(stream: &NamedPipeClient) -> io::Result<()> {
    let mut pid = 0;
    // SAFETY: stream is a valid client pipe and pid is writable.
    checked(unsafe { GetNamedPipeServerProcessId(stream.as_raw_handle(), &mut pid) })?;
    authenticate(pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE,
        Authorization::{ConvertStringSidToSidW, GetSecurityInfo, SE_FILE_OBJECT},
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetSecurityDescriptorControl,
        OWNER_SECURITY_INFORMATION, SE_DACL_PROTECTED,
    };

    #[tokio::test]
    async fn actual_pipe_acl_is_protected_and_grants_only_the_current_user() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let uppercase = directory.path().to_string_lossy().to_uppercase();
        assert_eq!(address(directory.path())?, address(Path::new(&uppercase))?);
        let listener = Listener::bind(directory.path()).await?;
        let pipe = listener.next.as_ref().unwrap();
        let mut dacl = null_mut();
        let mut descriptor = null_mut();
        // SAFETY: pipe is valid; both output pointers are writable. The OS owns
        // ACE pointers inside the returned LocalAlloc descriptor until drop.
        let code = unsafe {
            GetSecurityInfo(
                pipe.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut descriptor,
            )
        };
        anyhow::ensure!(code == 0, "{}", io::Error::from_raw_os_error(code as i32));
        let descriptor = LocalMemory(descriptor);
        anyhow::ensure!(!dacl.is_null(), "missing pipe DACL");
        let mut control = 0;
        let mut revision = 0;
        // SAFETY: descriptor came from GetSecurityInfo and outputs are writable.
        checked(unsafe {
            GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision)
        })?;
        assert_ne!(control & SE_DACL_PROTECTED, 0);
        // SAFETY: DACL is valid for the lifetime of descriptor.
        assert_eq!(unsafe { (*dacl).AceCount }, 1);
        let mut ace = null_mut();
        // SAFETY: index zero exists in this valid ACL and ace is writable.
        checked(unsafe { GetAce(dacl, 0, &mut ace) })?;
        // SAFETY: ACL returned an ACCESS_ALLOWED_ACE; its type is checked next.
        let ace = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        assert_eq!(ace.Header.AceType, 0); // ACCESS_ALLOWED_ACE_TYPE
        let sid: Vec<u16> = current_sid()?.encode_utf16().chain(Some(0)).collect();
        let mut expected = null_mut();
        // SAFETY: input is NUL-terminated and expected is writable.
        checked(unsafe { ConvertStringSidToSidW(sid.as_ptr(), &mut expected) })?;
        let expected = LocalMemory(expected);
        // SAFETY: this allow ACE contains its SID at SidStart, both SIDs remain alive.
        checked(unsafe { EqualSid((&ace.SidStart as *const u32).cast_mut().cast(), expected.0) })?;
        Ok(())
    }
}
