use orbolay_core::{app_state::AppHandle, config::TransportMode, util::bridge::BridgeMessage};
use orbolay_logging::error;

use crate::{ipc, ts3, ts6, websocket};

pub fn create_transport_thread(
  app: AppHandle,
  port: u16,
  force: Option<TransportMode>,
  transport_recv: flume::Receiver<BridgeMessage>,
) {
  std::thread::spawn(move || {
    let ws_port = port;
    let transport_mode =
      force.unwrap_or_else(|| app.read(|state| state.config.transport_mode.clone()));

    match transport_mode {
      TransportMode::Websocket => {
        app.update(|state| state.transport_mode = TransportMode::Websocket);
        if let Err(e) = websocket::create_websocket(ws_port, app.clone(), transport_recv) {
          error!("Websocket server failed on port {}: {}", ws_port, e);
        }
      }
      TransportMode::Ipc => {
        app.update(|state| state.transport_mode = TransportMode::Ipc);
        if let Err(e) = ipc::create_ipc_connection(app.clone(), transport_recv) {
          error!("IPC connection failed: {}", e);
        }
      }
      TransportMode::Ts6 => {
        app.update(|state| state.transport_mode = TransportMode::Ts6);
        if let Err(e) = ts6::create_ts6_client(app.clone(), transport_recv) {
          error!("TS6 client failed: {}", e);
        }
      }
      TransportMode::Ts3 => {
        app.update(|state| state.transport_mode = TransportMode::Ts3);
        if let Err(e) = ts3::create_ts3_client(app.clone(), transport_recv) {
          error!("TS3 client failed: {}", e);
        }
      }
      TransportMode::TsAuto => {
        // Try TS6 first, then fallback to TS3
        app.update(|state| state.transport_mode = TransportMode::Ts6);
        if let Err(e) = ts6::create_ts6_client(app.clone(), transport_recv.clone()) {
          error!("TS6 client failed, trying TS3: {}", e);
          app.update(|state| state.transport_mode = TransportMode::Ts3);
          if let Err(e) = ts3::create_ts3_client(app.clone(), transport_recv) {
            error!("TS3 client failed: {}", e);
          }
        }
      }
    }
  });
}
