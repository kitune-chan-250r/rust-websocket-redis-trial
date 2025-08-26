use axum::{
    Router,
    extract::{
        ConnectInfo, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
    routing::{any, get},
};
use futures::SinkExt;
use futures_util::stream::{SplitSink, SplitStream, StreamExt};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::{
    net::TcpListener,
    sync::{Mutex, mpsc, oneshot},
};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use uuid::Uuid;

type PeerKey = String;
type Peer = (SocketAddr, mpsc::Sender<Message>);

#[derive(Clone)]
struct AppState {
    peers: Arc<Mutex<HashMap<PeerKey, Peer>>>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let state = AppState {
        peers: Arc::new(Mutex::new(HashMap::new())),
    };

    // build our application with a route
    let app = Router::new()
        .route("/ws", any(websocket_handler))
        .route("/", get(health_check_handler))
        .with_state(state);

    // run it
    let listener = TcpListener::bind("0.0.0.0:8000").await.unwrap();
    tracing::info!("listening on {}", listener.local_addr().unwrap());
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
}

/**
 * health check用エンドポイント
 */
async fn health_check_handler() -> &'static str {
    "Hello, World!"
}

/**
 * WebSocket用エンドポイント
 */
async fn websocket_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    // WebSocketのハンドリング
    ws.on_upgrade(move |socket| handle_socket(socket, addr, state))
}

/**
 * 接続がアップグレードされた場合の処理
 */
async fn handle_socket(socket: WebSocket, socket_addr: SocketAddr, state: AppState) {
    // state.peers.lock().await.insert(socket_addr, so)

    // websocketの待ち受け処理
    let (sender, receiver) = socket.split();

    // よくわかってない、tokioのチャンネルについて理解する必要がありそう
    let (tx, rx) = mpsc::channel::<Message>(100);

    // ここもよくわかってない
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();

    // peerをappstateに登録
    // アプリ全体で任意のタイミングにpeerにメッセージを送信できるようにする
    let peer_key = add_peer(&state, socket_addr, tx).await;
    tracing::info!("new peer connected: {}", &peer_key);

    let send_task = tokio::spawn(send(sender, cancel_rx, rx));
    let recv_task = tokio::spawn(receive(receiver, cancel_tx, state.clone(), peer_key));

    tokio::select! {
        res = send_task => {
            if let Err(e) = res {
                eprintln!("Send task panicked: {:?}", e);
            }
        }
        res = recv_task => {
            if let Err(e) = res {
                eprintln!("Receive task panicked: {:?}", e);
            }
        }
    }
}

async fn receive(
    mut receiver: SplitStream<WebSocket>,
    cancel_tx: oneshot::Sender<()>,
    state: AppState,
    peer_key: PeerKey,
) {
    // ループの中でメッセージを受け取る処理
    while let Some(msg) = receiver.next().await {
        let msg = match msg {
            Ok(msg) => msg,
            Err(e) => {
                // メッセージ受信時や切断が発生した場合はループを抜ける
                tracing::error!("websocket error: {}", e);
                break;
            }
        };

        match msg {
            Message::Text(_) | Message::Binary(_) => {
                // 接続中のpeerリスト全員に対してsendする
                state.peers.lock().await.values().for_each(|(addr, tx)| {
                    tracing::info!("broadcasting message to {:?}", addr);
                    let _ = tx.try_send(msg.clone());
                });
            }

            Message::Close(_frame) => {
                // 絶対に他人へは送らない。自分の切断として処理して抜ける。
                tracing::info!("peer {} sent Close", peer_key);
                break;
            }

            _ => {}
        }
    }

    // ここに到達したら切断している
    remove_peer(&state, &peer_key).await;

    // 送信タスクをキャンセルする
    let _ = cancel_tx.send(());
}

async fn send(
    mut sender: SplitSink<WebSocket, Message>,
    mut cancel_rx: oneshot::Receiver<()>,
    mut rx: mpsc::Receiver<Message>,
) {
    loop {
        tokio::select! {
            // 他のpeerから送られてきたメッセージをsocketへ書き込み
            Some(msg) = rx.recv() => {
                if let Err(e) = sender.send(msg).await {
                    tracing::error!("failed to send message: {}", e);
                    break;
                }
            }

            // 切断検知 (receiveタスク側から通知が来る)
            _ = &mut cancel_rx => {
                tracing::info!("send task received cancel signal");
                break;
            }
        }
    }
}

/**
 * app stateにpeerを追加する
 */
async fn add_peer(
    state: &AppState,
    socket_addr: SocketAddr,
    peer: mpsc::Sender<Message>,
) -> String {
    let peer_key = Uuid::new_v4().to_string();
    let mut peers = state.peers.lock().await;
    tracing::info!("new peer connected: {}, addr: {}", &peer_key, &socket_addr);
    peers.insert(peer_key.clone(), (socket_addr, peer));
    peer_key
}

/**
 * app stateからpeerを削除する
 */
async fn remove_peer(state: &AppState, peer_key: &PeerKey) {
    let mut peers = state.peers.lock().await;
    if peers.remove(peer_key).is_some() {
        tracing::info!("peer disconnected: {}", peer_key);
    } else {
        tracing::warn!("attempted to remove non-existent peer: {}", peer_key);
    }
}
