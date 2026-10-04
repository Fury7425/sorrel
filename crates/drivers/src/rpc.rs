//! JSON-RPC 2.0 over a child's stdio, shared by the Codex and ACP drivers.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
    sync::{mpsc, oneshot},
};

/// A message the other side started.
#[derive(Debug)]
pub(crate) enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    /// Must be answered with [`Peer::respond`] or [`Peer::respond_error`].
    Request {
        id: Value,
        method: String,
        params: Value,
    },
}

type Reply = Result<Value, String>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Reply>>>>;

#[derive(Clone)]
pub(crate) struct Peer {
    out: mpsc::Sender<String>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
}

impl Peer {
    /// Starts the writer and reader tasks. The incoming channel closes, and
    /// every pending call fails, when the child's stdout closes.
    pub fn start(stdin: ChildStdin, stdout: ChildStdout) -> (Peer, mpsc::Receiver<Incoming>) {
        let (out, mut out_rx) = mpsc::channel::<String>(256);
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(mut line) = out_rx.recv().await {
                line.push('\n');
                if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
        });

        let pending: Pending = Arc::default();
        let (in_tx, in_rx) = mpsc::channel(1024);
        let replies = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                let incoming = match (msg["method"].as_str(), msg.get("id")) {
                    (Some(method), Some(id)) if !id.is_null() => Incoming::Request {
                        id: id.clone(),
                        method: method.to_owned(),
                        params,
                    },
                    (Some(method), _) => Incoming::Notification {
                        method: method.to_owned(),
                        params,
                    },
                    (None, Some(id)) => {
                        let reply = match msg.get("error") {
                            Some(error) if !error.is_null() => Err(error["message"]
                                .as_str()
                                .unwrap_or("request failed")
                                .to_owned()),
                            _ => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let waiter = id
                            .as_u64()
                            .and_then(|id| replies.lock().unwrap().remove(&id));
                        if let Some(waiter) = waiter {
                            let _ = waiter.send(reply);
                        }
                        continue;
                    }
                    (None, None) => continue,
                };
                if in_tx.send(incoming).await.is_err() {
                    break;
                }
            }
            let waiters: Vec<_> = replies.lock().unwrap().drain().collect();
            for (_, waiter) in waiters {
                let _ = waiter.send(Err("the agent process exited".into()));
            }
        });

        let peer = Peer {
            out,
            pending,
            next_id: Arc::new(AtomicU64::new(1)),
        };
        (peer, in_rx)
    }

    /// Sends a request; the receiver resolves with its result.
    // ponytail: a call whose reply never comes stays pending until the process exits.
    pub fn call(&self, method: &str, params: Value) -> oneshot::Receiver<Reply> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        rx
    }

    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Reply {
        match tokio::time::timeout(timeout, self.call(method, params)).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => Err("the agent process exited".into()),
            Err(_) => Err(format!("{method} timed out")),
        }
    }

    pub fn notify(&self, method: &str, params: Option<Value>) {
        match params {
            Some(params) => {
                self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            }
            None => self.send(json!({ "jsonrpc": "2.0", "method": method })),
        }
    }

    pub fn respond(&self, id: Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    pub fn respond_error(&self, id: Value, message: &str) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": message },
        }));
    }

    fn send(&self, msg: Value) {
        // A full queue means the child stopped reading; the watchdog handles that.
        let _ = self.out.try_send(msg.to_string());
    }
}

/// Waits for the next incoming message, or forever when there is no channel.
pub(crate) async fn next(incoming: &mut Option<mpsc::Receiver<Incoming>>) -> Option<Incoming> {
    match incoming {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}
