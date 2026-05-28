use concord_shared::protocol::{ClientMsg, Token};
use tokio::sync::mpsc;

use super::connection;
use super::types::{WsCommand, WsEvent};

#[derive(Clone)]
pub struct ConnectionHandle {
    cmd_tx: mpsc::Sender<WsCommand>,
}

impl ConnectionHandle {
    pub fn spawn(event_buffer: usize) -> (Self, mpsc::Receiver<WsEvent>) {
        let (cmd_tx, cmd_rx) = mpsc::channel::<WsCommand>(64);
        let (evt_tx, evt_rx) = mpsc::channel::<WsEvent>(event_buffer);

        tokio::spawn(connection::run(cmd_rx, evt_tx));

        (ConnectionHandle { cmd_tx }, evt_rx)
    }

    pub async fn connect(&self, url: String, token: Token) {
        let _ = self.cmd_tx.send(WsCommand::Connect { url, token }).await;
    }

    pub async fn send(&self, msg: ClientMsg) {
        let _ = self.cmd_tx.send(WsCommand::Send(msg)).await;
    }

    pub async fn shutdown(&self) {
        let _ = self.cmd_tx.send(WsCommand::Shutdown).await;
    }
}
