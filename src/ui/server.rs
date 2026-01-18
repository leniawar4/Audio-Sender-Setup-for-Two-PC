//! HTTP/WebSocket server for the web UI

use axum::{
    routing::{get, post},
    Router,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::broadcast;
use tower_http::cors::{Any, CorsLayer};

use crate::config::UiConfig;
use crate::protocol::ControlMessage;
use crate::tracks::TrackManager;
use crate::ui::handlers;
use crate::ui::websocket;
/// Shared application state
pub struct AppState {
    pub track_manager: Arc<TrackManager>,
    pub control_tx: broadcast::Sender<ControlMessage>,
    pub is_sender: bool,
}

impl AppState {
    pub fn new(track_manager: Arc<TrackManager>, is_sender: bool) -> Self {
        let (control_tx, _) = broadcast::channel(256);
        Self {
            track_manager,
            control_tx,
            is_sender,
        }
    }
    
    pub fn subscribe_control(&self) -> broadcast::Receiver<ControlMessage> {
        self.control_tx.subscribe()
    }
}

/// Web server for the control panel
pub struct WebServer {
    config: UiConfig,
    state: Arc<AppState>,
}

impl WebServer {
    /// Create a new web server
    pub fn new(config: UiConfig, track_manager: Arc<TrackManager>, is_sender: bool) -> Self {
        Self {
            config,
            state: Arc::new(AppState::new(track_manager, is_sender)),
        }
    }
    
    /// Get shared state
    pub fn state(&self) -> Arc<AppState> {
        self.state.clone()
    }
    
    /// Build the router
    fn build_router(&self) -> Router {
        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any);

        let mut router = Router::new()
            // API routes
            .route("/api/status", get(handlers::get_status))
            .route("/api/devices", get(handlers::get_devices))
            .route("/api/tracks", get(handlers::get_tracks))
            .route("/api/tracks", post(handlers::create_track))
            .route("/api/tracks/:id", axum::routing::delete(handlers::delete_track))
            .route("/api/tracks/:id", axum::routing::patch(handlers::update_track))
            .route("/api/tracks/:id/mute", post(handlers::set_mute))
            .route("/api/tracks/:id/solo", post(handlers::set_solo))
            .route("/api/tracks/:id/start", post(handlers::start_track))
            .route("/api/tracks/:id/stop", post(handlers::stop_track))
            // WebSocket
            .route("/ws", get(websocket::websocket_handler))
            // Health check
            .route("/health", get(|| async { "OK" }));

            use axum::{http::StatusCode, routing::get_service};
            use tower_http::services::ServeDir;

            // Serve static files for all non-API/non-WS paths
            let static_dir = self.config.static_dir.clone().unwrap_or_else(|| "static".into());
            router = router.nest_service("/", get_service(ServeDir::new(static_dir.clone())));

            // Fallback: serve index.html for any unknown path (SPA routing)
            use axum::{response::Response, http::header, body::Body};
            router = router.fallback(get(move || async move {
                let path = format!("{}/index.html", static_dir.display());
                match tokio::fs::read(&path).await {
                    Ok(bytes) => Response::builder()
                        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                        .body(Body::from(bytes))
                        .unwrap(),
                    Err(_) => Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .body(Body::from("index.html not found"))
                        .unwrap(),
                }
            }));

        router.layer(cors).with_state(self.state.clone())
    }
    
    /// Start the web server
    pub async fn start(&self) -> anyhow::Result<()> {
        let addr: SocketAddr = format!("{}:{}", self.config.bind_address, self.config.http_port)
            .parse()?;
        
        let router = self.build_router();
        
        tracing::info!("Web server listening on http://{}", addr);
        
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, router).await?;
        
        Ok(())
    }
    
    /// Start the web server in the background
    pub fn start_background(self) -> tokio::task::JoinHandle<anyhow::Result<()>> {
        tokio::spawn(async move {
            self.start().await
        })
    }
}
