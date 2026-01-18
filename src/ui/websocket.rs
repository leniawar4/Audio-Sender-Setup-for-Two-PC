//! WebSocket handler for real-time communication

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::protocol::{ControlMessage, DevicesResponse};
use crate::ui::server::AppState;

/// WebSocket upgrade handler
pub async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let is_sender = state.is_sender;
    ws.on_upgrade(move |socket| handle_socket(socket, state, is_sender))
}

/// Handle WebSocket connection
async fn handle_socket(socket: WebSocket, state: Arc<AppState>, is_sender: bool) {
    let (mut sender, mut receiver) = socket.split();
    
    // Subscribe to control messages
    let mut control_rx = state.control_tx.subscribe();
    let track_manager = state.track_manager.clone();
    let control_tx = state.control_tx.clone();
    
    // Send initial status
    let statuses = track_manager.get_all_statuses();
    let status_msg = ControlMessage::Status(statuses);
    if let Ok(json) = serde_json::to_string(&status_msg) {
        let _ = sender.send(Message::Text(json)).await;
    }
    
    // Spawn task to forward broadcast messages to WebSocket
    let mut send_task = tokio::spawn(async move {
        while let Ok(msg) = control_rx.recv().await {
            if let Ok(json) = serde_json::to_string(&msg) {
                if sender.send(Message::Text(json)).await.is_err() {
                    break;
                }
            }
        }
    });
    
    // Handle incoming messages
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            match msg {
                Message::Text(text) => {
                    if let Ok(control_msg) = serde_json::from_str::<ControlMessage>(&text) {
                        handle_control_message(control_msg, &track_manager, &control_tx, is_sender).await;
                    }
                }
                Message::Binary(_) => {
                    // Binary messages not supported
                }
                Message::Ping(_data) => {
                    // Pong is handled automatically by axum
                }
                Message::Pong(_) => {
                    // Ignore pongs
                }
                Message::Close(_) => {
                    break;
                }
            }
        }
    });
    
    // Wait for either task to complete
    tokio::select! {
        _ = &mut send_task => {
            recv_task.abort();
        }
        _ = &mut recv_task => {
            send_task.abort();
        }
    }
}

/// Handle incoming control message
async fn handle_control_message(
    msg: ControlMessage,
    track_manager: &Arc<crate::tracks::TrackManager>,
    control_tx: &broadcast::Sender<ControlMessage>,
    is_sender: bool,
) {
    match msg {
        ControlMessage::GetStatus => {
            let statuses = track_manager.get_all_statuses();
            let _ = control_tx.send(ControlMessage::Status(statuses));
        }
        
        ControlMessage::ListDevices => {
            let devices = crate::audio::device::list_devices();
            let resp = DevicesResponse { devices, is_receiver: !is_sender };
            let _ = control_tx.send(ControlMessage::Devices(resp));
        }
        
        ControlMessage::CreateTrack(config) => {
            match track_manager.create_track(config) {
                Ok(id) => {
                    tracing::info!("Created track {}", id);
                }
                Err(e) => {
                    let _ = control_tx.send(ControlMessage::Error {
                        message: e.to_string(),
                    });
                }
            }
        }
        
        ControlMessage::RemoveTrack { track_id } => {
            if let Err(e) = track_manager.remove_track(track_id) {
                let _ = control_tx.send(ControlMessage::Error {
                    message: e.to_string(),
                });
            }
        }
        
        ControlMessage::UpdateTrack { track_id, config } => {
            if let Err(e) = track_manager.update_track(track_id, config) {
                let _ = control_tx.send(ControlMessage::Error {
                    message: e.to_string(),
                });
            }
        }
        
        ControlMessage::SetMute { track_id, muted } => {
            if let Err(e) = track_manager.set_muted(track_id, muted) {
                let _ = control_tx.send(ControlMessage::Error {
                    message: e.to_string(),
                });
            }
        }
        
        ControlMessage::SetSolo { track_id, solo } => {
            if let Err(e) = track_manager.set_solo(track_id, solo) {
                let _ = control_tx.send(ControlMessage::Error {
                    message: e.to_string(),
                });
            }
        }
        
        ControlMessage::Ping => {
            let _ = control_tx.send(ControlMessage::Pong);
        }
        
        _ => {
            // Other messages are informational
        }
    }
}
