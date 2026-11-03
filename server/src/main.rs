use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use sharescreen_server::app;

fn env_port() -> u16 {
    std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080)
}

/// Dependency-free health probe: the container image ships no curl or wget,
/// so the server checks its own listening socket.
fn health_check(port: u16) -> i32 {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("health check failed: {e}");
            1
        }
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let port = env_port();

    if args.iter().any(|a| a == "--health-check") {
        std::process::exit(health_check(port));
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind signaling socket");

    tracing::info!("signaling server listening on {addr}");
    axum::serve(listener, app())
        .await
        .expect("signaling server failed");
}
