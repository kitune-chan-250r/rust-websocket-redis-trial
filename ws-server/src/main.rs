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
use redis::AsyncCommands;
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
    redis: Arc<RedisClient>,
    peers: Arc<Mutex<HashMap<PeerKey, Peer>>>,
}

// ----------------
// Infrastructure: Redis wrapper
// ----------------
struct RedisClient {
    client: redis::Client,
    channel: String,
}

impl RedisClient {
    async fn new(url: &str, channel: &str) -> anyhow::Result<Self> {
        let client = redis::Client::open(url)?;
        Ok(Self {
            client,
            channel: channel.to_string(),
        })
    }

    // publish JSON string
    async fn publish(&self, msg_json: String) -> anyhow::Result<()> {
        let mut conn = self.client.get_async_connection().await?;
        let _: () = conn
            .publish(self.channel.as_str(), msg_json)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(())
    }

    // returns a PubSub connection handle to be polled
    async fn subscribe(&self) -> anyhow::Result<redis::aio::PubSub> {
        let conn = self.client.get_async_connection().await?;
        let mut pubsub = conn.into_pubsub();
        pubsub
            .subscribe(self.channel.as_str())
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(pubsub)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    // redis setup
    let redis_url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://redis:6379".to_string());
    let channel = std::env::var("REDIS_CHANNEL").unwrap_or_else(|_| "ws:channel".to_string());

    let state = AppState {
        redis: Arc::new(RedisClient::new(&redis_url, &channel).await?),
        peers: Arc::new(Mutex::new(HashMap::new())),
    };

    // ws:channelへサブスクライブ開始
    start_redis_listener(state.clone()).await?;

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
    .await?;

    Ok(())
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
) -> anyhow::Result<()> {
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
                // state.peers.lock().await.values().for_each(|(addr, tx)| {
                //     tracing::info!("broadcasting message to {:?}", addr);
                //     let _ = tx.try_send(msg.clone());
                // })
                let msg_str = String::from(msg.to_text()?);
                tracing::info!("publish message");
                // redisへpublish
                let _ = state.redis.publish(msg_str).await;
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

    Ok(())
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

// redisから受け取ったメッセージを自鯖に接続している全peerへ送信
async fn start_redis_listener(state: AppState) -> anyhow::Result<()> {
    tokio::spawn(async move {
        loop {
            match state.redis.subscribe().await {
                Ok(mut pubsub) => {
                    tracing::info!("subscribed to Redis channel: {}", state.redis.channel);
                    let mut on_message = pubsub.on_message();

                    while let Some(msg) = on_message.next().await {
                        match msg.get_payload::<String>() {
                            Ok(payload_str) => {
                                // 受信そのものを必ずログ（peer 0 件でも見える）
                                tracing::debug!("redis message received: {}", payload_str);

                                // ロック中に送らない：先に送信先を集める（クローン）→ロック解放→送信
                                let senders: Vec<(SocketAddr, mpsc::Sender<Message>)> = {
                                    let peers = state.peers.lock().await;
                                    peers.values().cloned().collect()
                                };

                                for (addr, tx) in senders {
                                    tracing::info!("broadcasting message to {:?}", addr);
                                    let _ = tx.try_send(Message::Text(payload_str.clone().into()));
                                }
                            }
                            Err(e) => {
                                tracing::warn!("failed to decode redis payload: {}", e);
                            }
                        }
                    }

                    // on_message ストリームが終了した（接続落ちなど）
                    tracing::warn!("redis pubsub stream ended; will retry subscribe soon...");
                }
                Err(e) => {
                    // 起動直後に Redis 未起動・認証失敗等があるとここに来る
                    tracing::error!("redis subscribe failed: {} ; retrying...", e);
                }
            }

            // 速すぎる再試行を避け、一定時間後に再接続
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    });
    Ok(())
}
