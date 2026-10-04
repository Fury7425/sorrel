//! Daemon mode: the same Request/Update API over a local socket (a Unix
//! socket, or a named pipe on Windows), one JSON message per line, so agents
//! keep running when the window closes and other clients can attach later.
//!
//! The first line a client sends is a token the daemon wrote into the user's
//! data folder. Only someone who can read that folder can attach, even where
//! the socket itself is visible to other local users.

use std::{
    hash::{BuildHasher, Hasher},
    io,
    path::{Path, PathBuf},
};

use proto::{Request, Update};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{broadcast, mpsc},
};

use crate::Handle;

fn token_path(data_dir: &Path) -> PathBuf {
    data_dir.join("engine.token")
}

/// Where the daemon listens.
pub fn socket_path(data_dir: &Path) -> PathBuf {
    if cfg!(windows) {
        let user = std::env::var("USERNAME").unwrap_or_default();
        PathBuf::from(format!(r"\\.\pipe\sorrel-engine-{user}"))
    } else {
        data_dir.join("engine.sock")
    }
}

// ponytail: 128 bits from std's randomly keyed hasher; a CSPRNG crate if this ever leaves the machine.
fn new_token() -> String {
    let random = || {
        std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish()
    };
    format!(
        "{:016x}{:016x}",
        random(),
        random() ^ u64::from(std::process::id())
    )
}

/// Serves `handle` until the process exits.
pub async fn serve(handle: Handle, data_dir: &Path) -> io::Result<()> {
    let token = new_token();
    std::fs::write(token_path(data_dir), &token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(token_path(data_dir), std::fs::Permissions::from_mode(0o600))?;
    }
    let path = socket_path(data_dir);
    listen(handle, &path, token).await
}

#[cfg(unix)]
async fn listen(handle: Handle, path: &Path, token: String) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(serve_client(stream, handle.clone(), token.clone()));
    }
}

#[cfg(windows)]
async fn listen(handle: Handle, path: &Path, token: String) -> io::Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;
    let name = path.as_os_str();
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(name)?;
    loop {
        server.connect().await?;
        let connected = server;
        server = ServerOptions::new().create(name)?;
        tokio::spawn(serve_client(connected, handle.clone(), token.clone()));
    }
}

async fn serve_client<S>(stream: S, handle: Handle, token: String)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();
    match lines.next_line().await {
        Ok(Some(line)) if line.trim() == token => {}
        _ => return,
    }
    let requests = handle.requests();
    let mut updates = handle.subscribe();
    let resync = requests.clone();
    tokio::spawn(async move {
        loop {
            let update = match updates.recv().await {
                Ok(update) => update,
                // This client fell behind: send everything again.
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let _ = resync.send(Request::Hello).await;
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            let mut line = serde_json::to_string(&update).expect("updates serialize");
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    while let Ok(Some(line)) = lines.next_line().await {
        if let Ok(request) = serde_json::from_str::<Request>(&line)
            && requests.send(request).await.is_err()
        {
            break;
        }
    }
}

/// Attaches to a running daemon. Fails fast when none is listening.
pub async fn connect(
    data_dir: &Path,
) -> io::Result<(mpsc::Sender<Request>, mpsc::Receiver<Update>)> {
    let token = std::fs::read_to_string(token_path(data_dir))?;
    let path = socket_path(data_dir);
    #[cfg(unix)]
    let stream = tokio::net::UnixStream::connect(&path).await?;
    #[cfg(windows)]
    let stream = tokio::net::windows::named_pipe::ClientOptions::new().open(path.as_os_str())?;

    let (reader, mut writer) = tokio::io::split(stream);
    let (request_tx, mut request_rx) = mpsc::channel::<Request>(256);
    let (update_tx, update_rx) = mpsc::channel::<Update>(4096);
    tokio::spawn(async move {
        let mut line = token.trim().to_owned();
        line.push('\n');
        if writer.write_all(line.as_bytes()).await.is_err() {
            return;
        }
        while let Some(request) = request_rx.recv().await {
            let mut line = serde_json::to_string(&request).expect("requests serialize");
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(update) = serde_json::from_str::<Update>(&line)
                && update_tx.send(update).await.is_err()
            {
                break;
            }
        }
    });
    Ok((request_tx, update_rx))
}
