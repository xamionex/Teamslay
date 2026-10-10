use std::collections::HashMap;
use std::time::Duration;

use orbolay_core::{
  app_state::AppHandle,
  config::save_config,
  payloads::Notification,
  user::{User, UserVoiceState},
  util::{bridge::BridgeMessage, text::teamspeak_avatar_url},
};
use orbolay_logging::{error, log, warn};
use serde_json::{Value, json};
use tungstenite::{Message, Utf8Bytes, client::IntoClientRequest, connect as ws_connect};

const TS6_WS_URL_DEFAULT: &str = "ws://localhost:5899/";
const APP_IDENTIFIER: &str = "dex.teamspeak6.overlay";
const APP_VERSION: &str = "1.0.4";
const APP_NAME: &str = "Teamspeak Overlay";
const APP_DESCRIPTION: &str = "Shows speaking clients on screen";

/// A single TeamSpeak client, as tracked by the TS6 backend.
#[derive(Debug, Clone, Default)]
struct Ts6Client {
  id: String,
  nickname: String,
  avatar: String,
  channel_id: String,
  input_muted: bool,
  output_muted: bool,
  flag_talking: bool,
  is_streaming: bool,
}

impl Ts6Client {
  fn to_user(&self) -> User {
    let voice_state = if self.output_muted {
      UserVoiceState::Deafened
    } else if self.input_muted {
      UserVoiceState::Muted
    } else if self.flag_talking {
      UserVoiceState::Speaking
    } else {
      UserVoiceState::NotSpeaking
    };

    User {
      name: self.nickname.clone(),
      id: self.id.clone(),
      avatar: self.avatar.clone(),
      voice_state,
      streaming: self.is_streaming,
      camera: false,
      volume: 100.0,
      speaking_epoch: 0,
    }
  }
}

/// Everything the backend knows about the current connection.
#[derive(Debug, Default)]
struct Ts6State {
  self_id: String,
  current_channel: String,
  api_key: String,
  clients: HashMap<String, Ts6Client>,
}

impl Ts6State {
  /// Rebuild the overlay voice list from the tracked clients, keeping only
  /// those sitting in the current channel.
  fn sync(&self, app: &AppHandle) {
    let current_channel = self.current_channel.clone();
    let mut users: Vec<User> = self
      .clients
      .values()
      .filter(|c| c.channel_id == current_channel)
      .map(Ts6Client::to_user)
      .collect();

    users.sort_by(|a, b| a.id.cmp(&b.id));

    let self_id = self.self_id.clone();
    app.update(|state| {
      state.user_id = self_id;
      state.current_channel = current_channel;
      state.voice_users = users;
    });
  }
}

/// TS6 avatar properties look like `2,<url>`; the shared parser strips the tag.
fn clean_avatar(raw: &str) -> String {
  teamspeak_avatar_url(raw)
}

fn get_client_id(val: &Value) -> String {
  match val {
    Value::Number(n) => n.to_string(),
    Value::String(s) => s.clone(),
    _ => String::new(),
  }
}

/// Merge a TS6 `properties` object into an existing tracked client.
fn apply_properties(client: &mut Ts6Client, props: &Value) {
  if let Some(nickname) = props.get("nickname").and_then(|v| v.as_str()) {
    client.nickname = nickname.to_string();
  }
  if let Some(avatar) = props.get("myteamspeakAvatar").and_then(|v| v.as_str()) {
    client.avatar = clean_avatar(avatar);
  }
  if let Some(v) = props.get("inputMuted").and_then(|v| v.as_bool()) {
    client.input_muted = v;
  }
  if let Some(v) = props.get("outputMuted").and_then(|v| v.as_bool()) {
    client.output_muted = v;
  }
  if let Some(v) = props.get("flagTalking").and_then(|v| v.as_bool()) {
    client.flag_talking = v;
  }
  if let Some(v) = props.get("isStreaming").and_then(|v| v.as_bool()) {
    client.is_streaming = v;
  }
  if let Some(v) = props.get("channelId").and_then(|v| v.as_str()) {
    client.channel_id = v.to_string();
  }
}

fn handle_auth(payload: &Value, app: &AppHandle, state: &mut Ts6State) {
  if let Some(api_key) = payload
    .get("apiKey")
    .and_then(|v| v.as_str())
    .filter(|api_key| !api_key.is_empty() && state.api_key != *api_key)
  {
    state.api_key = api_key.to_string();
    app.update(|s| s.config.ts6_api_key = Some(api_key.to_string()));
    let config = app.read(|s| s.config.clone());
    save_config(&config);
  }

  let Some(connections) = payload.get("connections").and_then(|v| v.as_array()) else {
    return;
  };
  let Some(connection) = connections.first() else {
    return;
  };

  state.self_id = get_client_id(connection.get("clientId").unwrap_or(&Value::Null));

  state.clients.clear();

  if let Some(infos) = connection.get("clientInfos").and_then(|v| v.as_array()) {
    for info in infos {
      let id = get_client_id(info.get("id").unwrap_or(&Value::Null));
      if id.is_empty() {
        continue;
      }

      let channel_id = info
        .get("channelId")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

      let mut client = Ts6Client {
        id: id.clone(),
        channel_id,
        ..Default::default()
      };
      apply_properties(&mut client, info.get("properties").unwrap_or(&Value::Null));

      // The first clientInfo always carries the current channel.
      if state.current_channel.is_empty() {
        state.current_channel = client.channel_id.clone();
      }

      state.clients.insert(id, client);
    }
  }

  state.sync(app);
}

fn handle_talk_status(payload: &Value, app: &AppHandle, state: &mut Ts6State) {
  let id = get_client_id(payload.get("clientId").unwrap_or(&Value::Null));
  let status = payload.get("status").and_then(|v| v.as_i64()).unwrap_or(0) == 1;

  if let Some(client) = state.clients.get_mut(&id) {
    client.flag_talking = status;
    state.sync(app);
  }
}

fn handle_self_property(payload: &Value, app: &AppHandle, state: &mut Ts6State) {
  let Some(flag) = payload.get("flag").and_then(|v| v.as_str()) else {
    return;
  };
  let value = payload
    .get("newValue")
    .and_then(|v| v.as_bool())
    .unwrap_or(false);

  if let Some(client) = state.clients.get_mut(&state.self_id) {
    match flag {
      "flagTalking" => client.flag_talking = value,
      "inputMuted" => client.input_muted = value,
      "outputMuted" => client.output_muted = value,
      "isStreaming" => client.is_streaming = value,
      _ => return,
    }
    state.sync(app);
  }
}

fn handle_client_properties(payload: &Value, app: &AppHandle, state: &mut Ts6State) {
  let id = get_client_id(payload.get("clientId").unwrap_or(&Value::Null));
  if id.is_empty() {
    return;
  }

  let props = payload.get("properties").unwrap_or(&Value::Null);

  match state.clients.get_mut(&id) {
    Some(client) => apply_properties(client, props),
    None => {
      let mut client = Ts6Client {
        id: id.clone(),
        ..Default::default()
      };
      apply_properties(&mut client, props);
      state.clients.insert(id, client);
    }
  }

  state.sync(app);
}

fn handle_client_moved(payload: &Value, app: &AppHandle, state: &mut Ts6State) {
  let id = get_client_id(payload.get("clientId").unwrap_or(&Value::Null));
  let new_channel = payload
    .get("newChannelId")
    .and_then(|v| v.as_str())
    .unwrap_or_default()
    .to_string();

  if id.is_empty() {
    return;
  }

  // We moved: track the new channel and refresh the list. Our own row has to
  // follow too, otherwise sync()'s channel filter drops us from our own list.
  if id == state.self_id {
    state.current_channel = new_channel.clone();
  }

  match state.clients.get_mut(&id) {
    Some(client) => {
      client.channel_id = new_channel;
    }
    None => {
      // A client we have not seen yet moved into view; request its props.
      state.clients.insert(
        id.clone(),
        Ts6Client {
          id: id.clone(),
          channel_id: new_channel,
          ..Default::default()
        },
      );
    }
  }

  state.sync(app);
}

fn handle_text_message(payload: &Value, app: &AppHandle) {
  let invoker = payload.get("invoker").unwrap_or(&Value::Null);
  let nickname = invoker
    .get("nickname")
    .and_then(|v| v.as_str())
    .unwrap_or("Unknown")
    .to_string();
  let message = payload
    .get("message")
    .and_then(|v| v.as_str())
    .unwrap_or("")
    .to_string();

  if nickname.is_empty() && message.is_empty() {
    return;
  }

  app.notify(Notification {
    title: nickname,
    body: message,
    icon: String::new(),
    timestamp: Some(chrono::Utc::now().timestamp()),
    timeout_secs: 6,
    actions: None,
    guild_id: None,
    channel_id: None,
    message_id: None,
  });
}

fn handle_ts6_message(
  text: &str,
  app: &AppHandle,
  state: &mut Ts6State,
) -> Result<(), Box<dyn std::error::Error>> {
  let data: Value = serde_json::from_str(text)?;
  let msg_type = data.get("type").and_then(|v| v.as_str()).unwrap_or("");
  let payload = data.get("payload").unwrap_or(&Value::Null);

  match msg_type {
    "auth" => handle_auth(payload, app, state),
    "talkStatusChanged" => handle_talk_status(payload, app, state),
    "clientSelfPropertyUpdated" => handle_self_property(payload, app, state),
    "clientPropertiesUpdated" => handle_client_properties(payload, app, state),
    "clientMoved" => handle_client_moved(payload, app, state),
    "textMessage" => handle_text_message(payload, app),
    _ => {}
  }

  Ok(())
}

fn schedule_reconnect(reconnect_attempts: &mut u32, base: u64, max: u64) {
  let delay = base * (1u64 << (*reconnect_attempts).min(8));
  let delay = delay.min(max);
  *reconnect_attempts += 1;
  std::thread::sleep(Duration::from_millis(delay));
}

pub fn create_ts6_client(
  app: AppHandle,
  _transport_recv: flume::Receiver<BridgeMessage>,
) -> Result<(), Box<dyn std::error::Error>> {
  let mut reconnect_attempts: u32 = 0;
  let max_reconnect_delay: u64 = 30000;
  let base_reconnect_delay: u64 = 1000;

  loop {
    let (ws_url, api_key) = app.read(|state| {
      (
        state
          .config
          .ts6_ws_url
          .clone()
          .unwrap_or_else(|| TS6_WS_URL_DEFAULT.to_string()),
        state.config.ts6_api_key.clone().unwrap_or_default(),
      )
    });

    log!("Attempting to connect to TS6 WebSocket: {}", ws_url);

    let mut request = ws_url.clone().into_client_request()?;
    request.headers_mut().insert(
      "Origin",
      "file://"
        .parse()
        .unwrap_or_else(|_| "null".parse().unwrap()),
    );

    match ws_connect(request) {
      Ok((mut socket, _response)) => {
        log!("TS6 WebSocket connected");
        reconnect_attempts = 0;

        let auth_payload = json!({
          "type": "auth",
          "payload": {
            "identifier": APP_IDENTIFIER,
            "version": APP_VERSION,
            "name": APP_NAME,
            "description": APP_DESCRIPTION,
            "content": { "apiKey": api_key }
          }
        });

        if let Err(e) = socket.write(Message::Text(Utf8Bytes::from(auth_payload.to_string()))) {
          error!("Failed to send TS6 auth: {}", e);
          let _ = socket.close(None);
          schedule_reconnect(
            &mut reconnect_attempts,
            base_reconnect_delay,
            max_reconnect_delay,
          );
          continue;
        }
        let _ = socket.flush();

        let mut state = Ts6State {
          api_key,
          ..Default::default()
        };

        loop {
          match socket.read() {
            Ok(msg) => {
              if msg.is_close() {
                log!("TS6 WebSocket closed");
                app.update(|s| s.voice_users.clear());
                break;
              }
              if msg.is_text() || msg.is_binary() {
                let text = msg.to_text()?;
                log!("TS6 recv: {}", text);
                if let Err(e) = handle_ts6_message(text, &app, &mut state) {
                  error!("Failed to handle TS6 message: {}", e);
                }
              }
            }
            Err(e) => {
              warn!("TS6 WebSocket error: {}", e);
              break;
            }
          }
        }

        schedule_reconnect(
          &mut reconnect_attempts,
          base_reconnect_delay,
          max_reconnect_delay,
        );
      }
      Err(e) => {
        error!("TS6 connection failed: {}", e);
        app.notify(Notification {
          title: "TS6 Connection Failed".into(),
          body: format!(
            "Failed to connect to {}: {}. Accept the connection request in TeamSpeak.",
            ws_url, e
          ),
          icon: String::new(),
          timestamp: Some(chrono::Utc::now().timestamp()),
          timeout_secs: 5,
          actions: None,
          guild_id: None,
          channel_id: None,
          message_id: None,
        });
        schedule_reconnect(
          &mut reconnect_attempts,
          base_reconnect_delay,
          max_reconnect_delay,
        );
      }
    }
  }
}
