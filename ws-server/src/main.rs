use axum::{Router, routing::get};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() {
    // build our application with a route
    let app = Router::new().route("/", get(health_check_handler));

    // run it
    let listener = TcpListener::bind("0.0.0.0:8000").await.unwrap();
    tracing::debug!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

/**
 * health check用エンドポイント
 */
async fn health_check_handler() -> &'static str {
    "Hello, World!"
}
