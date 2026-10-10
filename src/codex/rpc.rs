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
    process::{Child, Command},
    sync::{Mutex, broadcast, mpsc, oneshot, watch},
};

pub const MAX_FRAME: usize = 1_048_576;
type Pending = oneshot::Sender<Result<Value>>;
struct Frame {
    bytes: Vec<u8>,
    reply: oneshot::Sender<Result<()>>,
}
struct Lease {
    recall: bool,
    turn: Option<String>,
    early: Vec<Value>,
}

pub struct Rpc {
    writer: mpsc::Sender<Frame>,
    child: Mutex<Child>,
    pending: Mutex<HashMap<u64, Pending>>,
    bound: Mutex<HashMap<String, Lease>>,
    events: broadcast::Sender<Value>,
    sequence: AtomicU64,
    stopped: AtomicBool,
    exited: watch::Sender<bool>,
    #[cfg(test)]
    outgoing: broadcast::Sender<Value>,
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
        let mut writer = child.stdin.take().context("No Codex stdin")?;
        let stdout = child.stdout.take().context("No Codex stdout")?;
        let stderr = child.stderr.take().context("No Codex stderr")?;
        let (events, _) = broadcast::channel(1024);
        let (writes, mut frames) = mpsc::channel::<Frame>(128);
        let (exited, _) = watch::channel(false);
        #[cfg(test)]
        let (outgoing, _) = broadcast::channel(128);
        let rpc = Arc::new(Self {
            writer: writes,
            child: Mutex::new(child),
            pending: Mutex::new(HashMap::new()),
            bound: Mutex::new(HashMap::new()),
            events,
            sequence: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
            exited,
            #[cfg(test)]
            outgoing,
        });
        let weak_writer = Arc::downgrade(&rpc);
        // A caller dropping its future cannot truncate a JSONL frame already queued here.
        tokio::spawn(async move {
            while let Some(frame) = frames.recv().await {
                let Some(rpc) = weak_writer.upgrade() else {
                    break;
                };
                let result = if rpc.closed() {
                    Err(anyhow::anyhow!("Codex worker closed"))
                } else {
                    tokio::time::timeout(Duration::from_secs(10), writer.write_all(&frame.bytes))
                        .await
                        .context("Codex write timed out")
                        .and_then(|r| r.map_err(Into::into))
                };
                let failure = result.as_ref().err().map(super::safe_error);
                let _ = frame.reply.send(result);
                if let Some(error) = failure {
                    rpc.fail(error).await;
                    break;
                }
            }
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
    #[cfg(test)]
    pub fn requests(&self) -> broadcast::Receiver<Value> {
        self.outgoing.subscribe()
    }
    #[cfg(test)]
    pub async fn inject(&self, message: Value) -> Result<()> {
        self.receive(message).await
    }
    pub async fn bind(&self, thread: &str, recall: bool) {
        self.bound.lock().await.insert(
            thread.into(),
            Lease {
                recall,
                turn: None,
                early: vec![],
            },
        );
    }
    pub async fn confirm_turn(&self, thread: &str, turn: &str) -> Result<()> {
        let early = {
            let mut bound = self.bound.lock().await;
            let lease = bound.get_mut(thread).context("Thread lease revoked")?;
            lease.turn = Some(turn.into());
            std::mem::take(&mut lease.early)
        };
        for message in early {
            if message["params"]["turnId"].as_str() == Some(turn) {
                let _ = self.events.send(message);
            } else {
                self.reject(&message).await?;
            }
        }
        Ok(())
    }
    pub async fn permits_recall(&self, thread: &str, turn: &str) -> bool {
        self.bound
            .lock()
            .await
            .get(thread)
            .is_some_and(|lease| lease.recall && lease.turn.as_deref() == Some(turn))
    }
    pub async fn unbind(&self, thread: &str) {
        let removed = self.bound.lock().await.remove(thread);
        if let Some(lease) = removed {
            for message in lease.early {
                let _ = self.reject(&message).await;
            }
        }
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
        #[cfg(test)]
        let _ = self.outgoing.send(message);
        let (reply, answer) = oneshot::channel();
        self.writer
            .send(Frame { bytes, reply })
            .await
            .context("Codex writer stopped")?;
        answer.await.context("Codex writer stopped")?
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
            let allowed = if message["method"] == "item/tool/call" && p["tool"] == "recall" {
                let mut bound = self.bound.lock().await;
                match p["threadId"]
                    .as_str()
                    .and_then(|thread| bound.get_mut(thread))
                {
                    Some(lease) if lease.recall && p["turnId"].is_string() => {
                        if let Some(turn) = &lease.turn {
                            p["turnId"].as_str() == Some(turn.as_str())
                        } else if lease.early.len() < 32 {
                            lease.early.push(message);
                            return Ok(());
                        } else {
                            false
                        }
                    }
                    _ => false,
                }
            } else {
                false
            };
            if allowed {
                let _ = self.events.send(message);
            } else {
                self.reject(&message).await?;
            }
        } else {
            let _ = self.events.send(message);
        }
        Ok(())
    }
    async fn reject(&self, message: &Value) -> Result<()> {
        self.send(json!({"id":message["id"],"error":{"code":-32601,"message":"Only recall for the active, confirmed NPC turn is available"}})).await
    }
    async fn fail(&self, error: String) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            let mut exited = self.exited.subscribe();
            let _ = exited.wait_for(|done| *done).await;
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
        self.exited.send_replace(true);
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
