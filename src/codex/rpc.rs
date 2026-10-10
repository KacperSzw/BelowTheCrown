use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, broadcast, oneshot},
};

pub const MAX_FRAME: usize = 1_048_576;
type Pending = oneshot::Sender<Result<Value>>;

pub struct Rpc {
    writer: Mutex<ChildStdin>,
    child: Mutex<Child>,
    pending: Mutex<HashMap<u64, Pending>>,
    bound: Mutex<HashMap<String, bool>>,
    events: broadcast::Sender<Value>,
    sequence: AtomicU64,
    stopped: AtomicBool,
}

pub async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Value>> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if bytes.is_empty() {
                return Ok(None);
            }
            bail!("Truncated protocol frame");
        }
        let end = available.iter().position(|b| *b == b'\n');
        let consumed = end.map_or(available.len(), |n| n + 1);
        if bytes.len() + consumed > MAX_FRAME {
            bail!("Protocol frame exceeds the limit");
        }
        bytes.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if end.is_some() {
            return Ok(Some(
                serde_json::from_slice(&bytes).context("Invalid JSON protocol frame")?,
            ));
        }
    }
}

impl Rpc {
    pub fn spawn(mut command: Command) -> Result<Arc<Self>> {
        let mut child = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("Cannot start isolated Codex worker")?;
        let writer = child.stdin.take().context("No Codex stdin")?;
        let stdout = child.stdout.take().context("No Codex stdout")?;
        let stderr = child.stderr.take().context("No Codex stderr")?;
        let (events, _) = broadcast::channel(1024);
        let rpc = Arc::new(Self {
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            pending: Mutex::new(HashMap::new()),
            bound: Mutex::new(HashMap::new()),
            events,
            sequence: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&rpc);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                let frame = read_frame(&mut reader).await;
                let Some(rpc) = weak.upgrade() else {
                    break;
                };
                match frame {
                    Ok(Some(message)) => {
                        if let Err(error) = rpc.receive(message).await {
                            rpc.fail(super::safe_error(error)).await;
                            break;
                        }
                    }
                    Ok(None) => {
                        rpc.fail("Codex worker exited".into()).await;
                        break;
                    }
                    Err(error) => {
                        rpc.fail(super::safe_error(error)).await;
                        break;
                    }
                }
            }
        });
        // Always drain stderr, retaining no credential-bearing raw output.
        tokio::spawn(async move {
            let mut stream = stderr;
            let mut bytes = [0u8; 4096];
            while stream.read(&mut bytes).await.is_ok_and(|n| n > 0) {}
        });
        Ok(rpc)
    }
    pub fn closed(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }
    pub async fn bind(&self, thread: &str, recall: bool) {
        self.bound.lock().await.insert(thread.into(), recall);
    }
    pub async fn unbind(&self, thread: &str) {
        self.bound.lock().await.remove(thread);
    }
    pub async fn send(&self, message: Value) -> Result<()> {
        if self.closed() {
            bail!("Codex worker is closed");
        }
        let mut bytes = serde_json::to_vec(&message)?;
        if bytes.len() > MAX_FRAME {
            bail!("Protocol request exceeds the limit");
        }
        bytes.push(b'\n');
        tokio::time::timeout(
            Duration::from_secs(10),
            self.writer.lock().await.write_all(&bytes),
        )
        .await
        .context("Codex write timed out")??;
        Ok(())
    }
    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send(json!({"method":method,"params":params})).await
    }
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let (send, recv) = oneshot::channel();
        self.pending.lock().await.insert(id, send);
        if let Err(error) = self
            .send(json!({"id":id,"method":method,"params":params}))
            .await
        {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        let result = tokio::time::timeout(Duration::from_secs(20), recv).await;
        self.pending.lock().await.remove(&id);
        result
            .with_context(|| format!("Codex {method} timed out"))?
            .context("Codex response channel closed")?
    }
    async fn receive(&self, message: Value) -> Result<()> {
        if message.get("method").is_none() {
            if let Some(id) = message["id"].as_u64()
                && let Some(pending) = self.pending.lock().await.remove(&id)
            {
                let result = if message.get("error").is_some() {
                    Err(anyhow::anyhow!(super::safe_error(
                        message["error"]["message"]
                            .as_str()
                            .unwrap_or("Codex protocol error")
                    )))
                } else {
                    Ok(message["result"].clone())
                };
                let _ = pending.send(result);
            }
        } else if message.get("id").is_some() {
            let p = &message["params"];
            let allowed = message["method"] == "item/tool/call"
                && p["tool"] == "recall"
                && match p["threadId"].as_str() {
                    Some(thread) => self.bound.lock().await.get(thread) == Some(&true),
                    None => false,
                };
            if allowed {
                let _ = self.events.send(message);
            } else {
                self.send(json!({"id":message["id"],"error":{"code":-32601,"message":"Only actor-bound recall is available"}})).await?;
            }
        } else {
            let _ = self.events.send(message);
        }
        Ok(())
    }
    async fn fail(&self, error: String) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        for (_, pending) in self.pending.lock().await.drain() {
            let _ = pending.send(Err(anyhow::anyhow!(error.clone())));
        }
        let _ = self
            .events
            .send(json!({"method":"__failure","params":{"error":error}}));
        let mut child = self.child.lock().await;
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    pub async fn close(&self) {
        self.fail("Codex worker closed".into()).await;
        self.bound.lock().await.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn fragmented_frames_and_unicode_are_decoded() {
        let (mut write, read) = tokio::io::duplex(32);
        tokio::spawn(async move {
            for piece in [
                b"{\"text\":\"".as_slice(),
                "Zażółć".as_bytes(),
                b"\"}\n{\"id\":2}\n",
            ] {
                write.write_all(piece).await.unwrap();
            }
        });
        let mut reader = BufReader::new(read);
        assert_eq!(
            read_frame(&mut reader).await.unwrap().unwrap()["text"],
            "Zażółć"
        );
        assert_eq!(read_frame(&mut reader).await.unwrap().unwrap()["id"], 2);
        assert!(read_frame(&mut reader).await.unwrap().is_none());
    }
    #[tokio::test]
    async fn malformed_and_oversized_frames_are_rejected() {
        let mut bad = BufReader::new(&b"not json\n"[..]);
        assert!(read_frame(&mut bad).await.is_err());
        let bytes = vec![b'x'; MAX_FRAME + 1];
        let mut huge = BufReader::new(bytes.as_slice());
        assert!(read_frame(&mut huge).await.is_err());
        let mut incomplete = BufReader::new(&b"{\"id\":2}"[..]);
        assert!(read_frame(&mut incomplete).await.is_err());
    }
}
