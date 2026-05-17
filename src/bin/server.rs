use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast::{channel, Sender};
use tokio::sync::Mutex;
use tokio_websockets::{Message, ServerBuilder, WebSocketStream};

#[derive(Clone, Debug)]
struct ClientInfo {
    name: Option<String>,
}

#[derive(Clone)]
struct ServerState {
    clients: Arc<Mutex<HashMap<SocketAddr, ClientInfo>>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum MsgTypes {
    Users,
    Register,
    Message,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebSocketMessage {
    message_type: MsgTypes,
    data_array: Option<Vec<String>>,
    data: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ChatPayload {
    from: String,
    message: String,
    time: u64,
}

impl ServerState {
    fn new() -> Self {
        Self {
            clients: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn register_connection(&self, addr: SocketAddr) {
        self.clients
            .lock()
            .await
            .insert(addr, ClientInfo { name: None });
    }

    async fn set_name(&self, addr: SocketAddr, name: String) {
        if let Some(client) = self.clients.lock().await.get_mut(&addr) {
            client.name = Some(name);
        }
    }

    async fn remove_connection(&self, addr: SocketAddr) {
        self.clients.lock().await.remove(&addr);
    }

    async fn names(&self) -> Vec<String> {
        let clients = self.clients.lock().await;
        clients
            .values()
            .filter_map(|client| client.name.clone())
            .collect()
    }

    async fn display_name(&self, addr: SocketAddr) -> String {
        let clients = self.clients.lock().await;
        clients
            .get(&addr)
            .and_then(|client| client.name.clone())
            .unwrap_or_else(|| addr.to_string())
    }
}

fn users_message(users: Vec<String>) -> String {
    serde_json::to_string(&WebSocketMessage {
        message_type: MsgTypes::Users,
        data_array: Some(users),
        data: None,
    })
    .expect("users message to serialize")
}

fn chat_message(from: String, message: String) -> String {
    let payload = ChatPayload {
        from,
        message,
        time: 0,
    };

    serde_json::to_string(&WebSocketMessage {
        message_type: MsgTypes::Message,
        data_array: None,
        data: Some(
            serde_json::to_string(&payload).expect("chat payload to serialize"),
        ),
    })
    .expect("message wrapper to serialize")
}

async fn broadcast_users(
    state: &ServerState,
    bcast_tx: &Sender<String>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let users = state.names().await;
    bcast_tx.send(users_message(users))?;
    Ok(())
}

async fn handle_incoming_text(
    addr: SocketAddr,
    text: &str,
    state: &ServerState,
    bcast_tx: &Sender<String>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    if let Ok(parsed) = serde_json::from_str::<WebSocketMessage>(text) {
        match parsed.message_type {
            MsgTypes::Register => {
                if let Some(name) = parsed.data {
                    state.set_name(addr, name).await;
                    broadcast_users(state, bcast_tx).await?;
                }
            }
            MsgTypes::Message => {
                let from = state.display_name(addr).await;
                let message = parsed.data.unwrap_or_default();
                println!("From client {from}: {:?}", message);
                bcast_tx.send(chat_message(from, message))?;
            }
            MsgTypes::Users => {}
        }
    } else {
        let from = state.display_name(addr).await;
        println!("From client {from}: {:?}", text);
        bcast_tx.send(chat_message(from, text.to_string()))?;
    }

    Ok(())
}

async fn handle_connection(
    addr: SocketAddr,
    ws_stream: WebSocketStream<TcpStream>,
    bcast_tx: Sender<String>,
    state: ServerState,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut bcast_rx = bcast_tx.subscribe();
    let (mut ws_write, mut ws_read) = ws_stream.split();
    state.register_connection(addr).await;

    loop {
        tokio::select! {
            msg = ws_read.next() => match msg {
                Some(Ok(msg)) => {
                    if let Some(text) = msg.as_text() {
                        handle_incoming_text(addr, text, &state, &bcast_tx).await?;
                    }
                }
                Some(Err(err)) => return Err(Box::new(err)),
                None => break,
            },
            msg = bcast_rx.recv() => match msg {
                Ok(text) => ws_write.send(Message::text(text)).await?,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }

    state.remove_connection(addr).await;
    broadcast_users(&state, &bcast_tx).await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let state = ServerState::new();
    let (bcast_tx, _) = channel(32);

    let listener = TcpListener::bind("127.0.0.1:8080").await?;
    println!("listening on port 8080");

    loop {
        let (socket, addr) = listener.accept().await?;
        println!("New connection from {addr}");
        let bcast_tx = bcast_tx.clone();
        let state = state.clone();

        tokio::spawn(async move {
            let (_req, ws_stream) = ServerBuilder::new().accept(socket).await?;
            handle_connection(addr, ws_stream, bcast_tx, state).await
        });
    }
}
