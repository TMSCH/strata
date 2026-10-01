use crate::{Append, Receipt, Store, record::MAX_REQUEST};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    receipt: Option<Receipt>,
    error: Option<String>,
}

fn read_line(stream: &UnixStream) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    BufReader::new(stream)
        .take((MAX_REQUEST + 2) as u64)
        .read_until(b'\n', &mut bytes)?;
    ensure!(
        bytes.ends_with(b"\n") && bytes.len() <= MAX_REQUEST + 1,
        "expected one JSON line of at most 64 KiB"
    );
    Ok(bytes)
}

fn handle(mut stream: UnixStream, store: &Mutex<Store>) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let result = (|| -> Result<Receipt> {
        let bytes = read_line(&stream)?;
        let request: Append = serde_json::from_slice(&bytes)?;
        store
            .lock()
            .map_err(|_| anyhow::anyhow!("writer lock poisoned"))?
            .append(request)
    })();
    let response = match result {
        Ok(receipt) => Response {
            receipt: Some(receipt),
            error: None,
        },
        Err(error) => Response {
            receipt: None,
            error: Some(format!("{error:#}")),
        },
    };
    let mut bytes = serde_json::to_vec(&response)?;
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    Ok(())
}

/// Blocking local server with four fixed workers and one serialized writer.
/// Socket parent directory must be private and controlled by the operator.
pub fn serve(mut store: Store, socket: &Path) -> Result<()> {
    let parent = socket
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let metadata = fs::symlink_metadata(parent)?;
    ensure!(
        metadata.is_dir() && metadata.permissions().mode() & 0o022 == 0,
        "socket parent must be a real directory not writable by group or others"
    );
    let mut lock_path = socket.as_os_str().to_os_string();
    lock_path.push(".lock");
    let socket_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(lock_path)?;
    ensure!(
        socket_lock.metadata()?.is_file() && socket_lock.metadata()?.nlink() == 1,
        "invalid socket lock"
    );
    socket_lock
        .try_lock()
        .context("socket is already owned by another daemon")?;
    match fs::symlink_metadata(socket) {
        Ok(meta) => {
            ensure!(
                meta.file_type().is_socket(),
                "refusing to replace a non-socket path"
            );
            match UnixStream::connect(socket) {
                Ok(_) => anyhow::bail!("socket is already serving"),
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    fs::remove_file(socket)?
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener =
        UnixListener::bind(socket).context("bind Unix socket (use a short absolute path)")?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    store.compact()?;
    let store = Arc::new(Mutex::new(store));
    eprintln!("strata: listening on {}", socket.display());
    let listeners = (0..4)
        .map(|_| listener.try_clone())
        .collect::<std::io::Result<Vec<_>>>()?;
    thread::scope(|scope| -> Result<()> {
        let maintenance = Arc::clone(&store);
        scope.spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(60));
                let Ok(mut store) = maintenance.lock() else {
                    break;
                };
                if let Err(error) = store.compact() {
                    eprintln!("strata: compaction stopped: {error:#}");
                    break;
                }
            }
        });
        for listener in listeners {
            let store = Arc::clone(&store);
            scope.spawn(move || {
                for connection in listener.incoming() {
                    match connection {
                        Ok(stream) => {
                            if let Err(e) = handle(stream, &store) {
                                eprintln!("strata: client connection: {e:#}");
                            }
                        }
                        Err(e) => {
                            eprintln!("strata: accept: {e}");
                            thread::sleep(Duration::from_millis(100));
                        }
                    }
                }
            });
        }
        Ok(())
    })
}

pub fn append(socket: &Path, request: &Append) -> Result<Receipt> {
    request.validate()?;
    let mut stream = UnixStream::connect(socket).context("connect to Strata daemon")?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut bytes = serde_json::to_vec(request)?;
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    let response: Response = serde_json::from_slice(&read_line(&stream)?)?;
    match (response.receipt, response.error) {
        (Some(receipt), None) => Ok(receipt),
        (None, Some(error)) => anyhow::bail!("{error}"),
        _ => anyhow::bail!("invalid daemon response"),
    }
}
