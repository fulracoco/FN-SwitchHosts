//! The privileged process exposes only read / compare-and-swap of /etc/hosts.
use crate::model::MAX_TOTAL;
use anyhow::{bail, ensure, Context, Result};
#[cfg(unix)]
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub enum Target {
    DevelopmentFile(PathBuf),
    #[cfg(unix)]
    WriterSocket(PathBuf),
}

#[cfg(unix)]
pub fn secure_socket_parent(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let parent = std::fs::canonicalize(path.parent().context("socket path needs a parent")?)?;
    let metadata = std::fs::metadata(&parent)?;
    ensure!(
        metadata.is_dir() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
        "socket parent must be a root-owned directory without group/other write permission: {} (uid={}, mode={:04o})",
        parent.display(),
        metadata.uid(),
        metadata.mode() & 0o7777
    );
    Ok(())
}

#[cfg(unix)]
#[derive(Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Read,
    Write { expected: String, content: String },
}

#[cfg(unix)]
#[derive(Deserialize, Serialize)]
struct Reply {
    content: Option<String>,
    error: Option<String>,
}

pub fn read_file(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "hosts target must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take(MAX_TOTAL as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_TOTAL, "hosts target exceeds 16 MiB");
    let content = String::from_utf8(bytes)
        .context("NAS hosts must be UTF-8; original file was not modified")?;
    crate::hosts_text::validate_system_text(&content)?;
    Ok(content)
}

pub fn compare_and_write(path: &Path, expected: &str, content: &str) -> Result<()> {
    ensure!(
        content.len() <= MAX_TOTAL && expected.len() <= MAX_TOTAL,
        "hosts exceeds 16 MiB"
    );
    crate::hosts_text::validate_system_text(content)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "hosts target must be a regular file"
    );
    fs2::FileExt::try_lock_exclusive(&file).context("hosts file is locked by another writer")?;
    let mut current = Vec::new();
    (&mut file)
        .take(MAX_TOTAL as u64 + 1)
        .read_to_end(&mut current)?;
    ensure!(
        current == expected.as_bytes(),
        "conflict: hosts changed outside FN-SwitchHosts; reload and retry"
    );
    if current == content.as_bytes() {
        return Ok(());
    }
    let result = (|| -> std::io::Result<()> {
        file.seek(SeekFrom::Start(0))?;
        file.write_all(content.as_bytes())?;
        file.set_len(content.len() as u64)?;
        file.sync_all()
    })();
    if let Err(error) = result {
        let restored = (|| -> std::io::Result<()> {
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&current)?;
            file.set_len(current.len() as u64)?;
            file.sync_all()
        })();
        bail!("hosts write failed: {error}; compensation: {restored:?}");
    }
    Ok(())
}

impl Target {
    pub async fn read(&self) -> Result<String> {
        match self {
            Self::DevelopmentFile(path) => read_file(path),
            #[cfg(unix)]
            Self::WriterSocket(path) => exchange(path, Request::Read).await,
        }
    }

    pub async fn write(&self, expected: &str, content: &str) -> Result<()> {
        match self {
            Self::DevelopmentFile(path) => compare_and_write(path, expected, content),
            #[cfg(unix)]
            Self::WriterSocket(path) => {
                exchange(
                    path,
                    Request::Write {
                        expected: expected.into(),
                        content: content.into(),
                    },
                )
                .await?;
                Ok(())
            }
        }
    }
}

#[cfg(unix)]
async fn exchange(path: &Path, request: Request) -> Result<String> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut socket = tokio::net::UnixStream::connect(path).await?;
        let mut bytes = serde_json::to_vec(&request)?;
        bytes.push(b'\n');
        socket.write_all(&bytes).await?;
        let mut response = Vec::new();
        BufReader::new(socket.take((MAX_TOTAL * 8) as u64))
            .read_until(b'\n', &mut response)
            .await?;
        let reply: Reply = serde_json::from_slice(&response)?;
        if let Some(error) = reply.error {
            bail!("{error}");
        }
        Ok(reply.content.unwrap_or_default())
    })
    .await
    .context("hosts writer timed out; pending operation will be reconciled")?
}

#[cfg(unix)]
pub async fn run_writer(path: &Path, uid: u32, gid: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    ensure!(
        unsafe { libc::geteuid() } == 0,
        "hosts writer must run as root"
    );
    secure_socket_parent(path)?;
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let path_c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    ensure!(
        unsafe { libc::chown(path_c.as_ptr(), uid, gid) } == 0,
        "cannot set writer socket owner"
    );
    loop {
        let (mut socket, _) = listener.accept().await?;
        // Socket mode + kernel peer credentials: no trust in JSON-supplied identity.
        if socket.peer_cred()?.uid() != uid {
            continue;
        }
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut bytes = Vec::new();
            BufReader::new((&mut socket).take((MAX_TOTAL * 16 + 1024) as u64))
                .read_until(b'\n', &mut bytes)
                .await?;
            let reply = match serde_json::from_slice::<Request>(&bytes) {
                Ok(Request::Read) => read_file(Path::new("/etc/hosts")),
                Ok(Request::Write { expected, content }) => {
                    compare_and_write(Path::new("/etc/hosts"), &expected, &content)
                        .map(|()| String::new())
                }
                Err(error) => Err(error.into()),
            };
            let reply = match reply {
                Ok(content) => Reply {
                    content: Some(content),
                    error: None,
                },
                Err(error) => Reply {
                    content: None,
                    error: Some(error.to_string()),
                },
            };
            let mut bytes = serde_json::to_vec(&reply)?;
            bytes.push(b'\n');
            socket.write_all(&bytes).await?;
            Ok::<_, anyhow::Error>(())
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!("writer connection failed: {error}"),
            Err(error) => eprintln!("writer connection timed out: {error}"),
        }
    }
}
