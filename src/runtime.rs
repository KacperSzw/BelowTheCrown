use anyhow::{Result, anyhow};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};
use tokio::sync::{mpsc, oneshot, watch};

pub type Reply = oneshot::Sender<Result<Value>>;

pub enum RuntimeRequest {
    Join {
        identity: Option<String>,
        name: String,
        reply: oneshot::Sender<Result<PlayerConnection>>,
    },
    Player {
        actor_id: u16,
        session_id: u64,
        method: String,
        params: Value,
        reply: Reply,
    },
    Disconnect {
        actor_id: u16,
        session_id: u64,
    },
    Manager {
        method: String,
        params: Value,
        reply: Reply,
    },
}

#[derive(Clone, Debug)]
pub struct HostInfo {
    pub data_dir: PathBuf,
    pub address: String,
    pub port: u16,
    pub maximum_players: usize,
}

#[derive(Clone)]
pub struct GameHandle {
    pub sender: mpsc::Sender<RuntimeRequest>,
    pub manager: watch::Receiver<Value>,
    pub info: Arc<HostInfo>,
}

pub struct PlayerConnection {
    pub actor_id: u16,
    pub session_id: u64,
    pub views: watch::Receiver<Value>,
    pub handle: GameHandle,
}

impl GameHandle {
    pub async fn join(&self, identity: Option<String>, name: String) -> Result<PlayerConnection> {
        let (reply, answer) = oneshot::channel();
        self.sender
            .send(RuntimeRequest::Join {
                identity,
                name,
                reply,
            })
            .await
            .map_err(|_| anyhow!("Server stopped"))?;
        answer.await.map_err(|_| anyhow!("Server stopped"))?
    }

    pub async fn request(&self, method: impl Into<String>, params: Value) -> Result<Value> {
        let (reply, answer) = oneshot::channel();
        self.sender
            .send(RuntimeRequest::Manager {
                method: method.into(),
                params,
                reply,
            })
            .await
            .map_err(|_| anyhow!("Server stopped"))?;
        answer.await.map_err(|_| anyhow!("Server stopped"))?
    }
}

impl PlayerConnection {
    pub async fn request(&self, method: impl Into<String>, params: Value) -> Result<Value> {
        let (reply, answer) = oneshot::channel();
        self.handle
            .sender
            .send(RuntimeRequest::Player {
                actor_id: self.actor_id,
                session_id: self.session_id,
                method: method.into(),
                params,
                reply,
            })
            .await
            .map_err(|_| anyhow!("Server stopped"))?;
        answer.await.map_err(|_| anyhow!("Server stopped"))?
    }

    pub async fn disconnect(&self) {
        let _ = self
            .handle
            .sender
            .send(RuntimeRequest::Disconnect {
                actor_id: self.actor_id,
                session_id: self.session_id,
            })
            .await;
    }
}
