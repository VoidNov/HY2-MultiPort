#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
use std::{io, path::Path};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

use crate::state::RuntimeState;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "command", rename_all = "lowercase")]
pub enum Request {
    Apply,
    Status,
    Logs,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogEvent {
    pub timestamp_unix: u64,
    pub level: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Response {
    pub ok: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<RuntimeState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logs: Option<Vec<LogEvent>>,
}

impl Response {
    #[must_use]
    pub fn success(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: message.into(),
            state: None,
            logs: None,
        }
    }

    #[must_use]
    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: message.into(),
            state: None,
            logs: None,
        }
    }
}

/// Removes a stale socket, binds the control listener, and sets its mode.
///
/// # Errors
///
/// Returns an error when the path cannot be inspected, a stale socket cannot
/// be removed, the listener cannot be bound, or its permissions cannot be set.
pub fn bind_socket(path: impl AsRef<Path>) -> Result<UnixListener> {
    let path = path.as_ref();
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(path)
            .with_context(|| format!("cannot remove stale socket {}", path.display()))?,
        Ok(_) => bail!(
            "refusing to replace non-socket control path {}",
            path.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("cannot inspect socket {}", path.display()));
        }
    }
    let listener = UnixListener::bind(path)
        .with_context(|| format!("cannot bind socket {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
            .with_context(|| format!("cannot set socket permissions on {}", path.display()))?;
    }
    Ok(listener)
}

/// Reads and deserializes one newline-delimited control request.
///
/// # Errors
///
/// Returns an error when reading fails, the request is empty or too large, or
/// its JSON payload is invalid.
pub async fn receive_request(stream: &mut UnixStream) -> Result<Request> {
    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    let count = reader.read_line(&mut line).await?;
    if count == 0 || count > 16 * 1024 {
        bail!("invalid control request length");
    }
    serde_json::from_str(line.trim_end()).context("invalid control request JSON")
}

/// Serializes and writes one newline-delimited control response.
///
/// # Errors
///
/// Returns an error when serialization, writing, or closing the stream fails.
pub async fn send_response(stream: &mut UnixStream, response: &Response) -> Result<()> {
    let encoded = serde_json::to_vec(response)?;
    stream.write_all(&encoded).await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;
    Ok(())
}

/// Sends a request to a daemon control socket and reads its response.
///
/// # Errors
///
/// Returns an error when the socket cannot be used, either JSON payload cannot
/// be serialized or deserialized, or the daemon rejects the request.
pub async fn call(path: impl AsRef<Path>, request: &Request) -> Result<Response> {
    let path = path.as_ref();
    let mut stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("cannot connect to port-forwardd at {}", path.display()))?;
    let request = serde_json::to_vec(request)?;
    stream.write_all(&request).await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;
    let mut line = String::new();
    BufReader::new(&mut stream).read_line(&mut line).await?;
    let response: Response =
        serde_json::from_str(line.trim_end()).context("invalid response from port-forwardd")?;
    if response.ok {
        Ok(response)
    } else {
        bail!("daemon rejected request: {}", response.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn socket_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("control.sock");
        let listener = bind_socket(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert_eq!(receive_request(&mut stream).await.unwrap(), Request::Status);
            send_response(&mut stream, &Response::success("ok"))
                .await
                .unwrap();
        });
        assert_eq!(call(&socket, &Request::Status).await.unwrap().message, "ok");
        server.await.unwrap();
    }
}
