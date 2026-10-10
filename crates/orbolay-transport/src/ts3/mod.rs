use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use md5::{Digest, Md5};
use orbolay_core::{
  app_state::AppHandle,
  config::config_dir,
  payloads::Notification,
  user::{User, UserVoiceState},
  util::{bridge::BridgeMessage, text::teamspeak_avatar_url},
};
use orbolay_logging::{error, log, warn};

const TS3_HOST_DEFAULT: &str = "127.0.0.1";
const TS3_PORT_DEFAULT: u16 = 25639;

/// `whoami` reports this while the client is not attached to a server. Leaving
/// a server does not close the ClientQuery connection, so this is the only way
/// we can notice that we dropped out.
const TS3_ERROR_NOT_CONNECTED: u32 = 1794;

/// Client variables we pull for every tracked client. Between them the two
/// avatar properties cover everything ClientQuery exposes: `client_flag_avatar`
/// is the MD5 of the avatar the server holds, while `client_myteamspeak_avatar`
/// carries `<mode>,<url>` exactly like TS6's `myteamspeakAvatar`.
const CLIENT_VARIABLES: &str = "client_nickname client_flag_talking client_input_muted \
client_output_muted client_away client_input_hardware client_output_hardware \
client_is_channel_commander client_talk_power client_servergroups client_icon_id \
client_country client_version client_platform client_is_priority_speaker \
client_is_recording client_flag_avatar client_myteamspeak_avatar";

#[derive(Debug, Clone, Default)]
struct Ts3Client {
  id: String,
  nickname: String,
  avatar: String,
  channel_id: String,
  input_muted: bool,
  output_muted: bool,
  flag_talking: bool,
  away: bool,
  is_recording: bool,
}

impl Ts3Client {
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
      streaming: false,
      camera: false,
      volume: 100.0,
      speaking_epoch: 0,
    }
  }
}

#[derive(Debug, Default)]
struct Ts3State {
  self_id: String,
  current_channel: String,
  clients: HashMap<String, Ts3Client>,
}

impl Ts3State {
  fn sync(&self, app: &AppHandle) {
    let current_channel = self.current_channel.clone();
    let mut users: Vec<User> = self
      .clients
      .values()
      .filter(|c| c.channel_id == current_channel)
      .map(Ts3Client::to_user)
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

/// Decode TeamSpeak's escape sequences in a single pass so that eg. a literal
/// `\n` inside a message is not double-decoded.
fn decode_ts3(input: &str) -> String {
  let mut out = String::with_capacity(input.len());
  let mut chars = input.chars();

  while let Some(c) = chars.next() {
    if c != '\\' {
      out.push(c);
      continue;
    }

    match chars.next() {
      Some('s') => out.push(' '),
      Some('p') => out.push('|'),
      Some('/') => out.push('/'),
      Some('\\') => out.push('\\'),
      Some('a') => out.push('\x07'),
      Some('b') => out.push('\x08'),
      Some('f') => out.push('\x0C'),
      Some('n') => out.push('\n'),
      Some('r') => out.push('\r'),
      Some('t') => out.push('\t'),
      Some('v') => out.push('\x0B'),
      Some(other) => {
        out.push('\\');
        out.push(other);
      }
      None => out.push('\\'),
    }
  }

  out
}

/// Parse a `key=value key=value` line into ordered pairs. Real spaces inside
/// values arrive escaped as `\s`, so splitting on literal spaces is safe.
fn parse_params(line: &str) -> Vec<(String, String)> {
  line
    .split(' ')
    .filter(|tok| !tok.is_empty())
    .filter_map(|tok| tok.split_once('='))
    .map(|(k, v)| (k.to_string(), decode_ts3(v)))
    .collect()
}

fn get<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
  params
    .iter()
    .find(|(k, _)| k == key)
    .map(|(_, v)| v.as_str())
}

/// Directories where TeamSpeak clients keep the avatars they have downloaded.
/// Each root holds one folder per server, and every server folder keeps its
/// avatars under `clients/`.
fn avatar_cache_roots() -> Vec<PathBuf> {
  let mut roots = Vec::new();

  if let Some(home) = dirs::home_dir() {
    roots.push(home.join(".ts3client").join("cache"));
  }
  if let Some(cache) = dirs::cache_dir() {
    roots.push(cache.join("TeamSpeak").join("Default"));
  }
  if let Some(config) = dirs::config_dir() {
    roots.push(config.join("TS3Client").join("cache"));
  }

  roots
}

fn md5_hex(bytes: &[u8]) -> String {
  let mut hasher = Md5::new();
  hasher.update(bytes);
  hasher
    .finalize()
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect()
}

/// Index every cached avatar by the MD5 of its bytes.
fn scan_cached_avatars() -> HashMap<String, PathBuf> {
  let mut found = HashMap::new();

  for root in avatar_cache_roots() {
    let Ok(servers) = std::fs::read_dir(root) else {
      continue;
    };
    for server in servers.flatten() {
      let Ok(entries) = std::fs::read_dir(server.path().join("clients")) else {
        continue;
      };
      for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
          continue;
        };
        if !name.starts_with("avatar_") {
          continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
          continue;
        };
        found.entry(md5_hex(&bytes)).or_insert(path);
      }
    }
  }

  found
}

struct AvatarIndex {
  scanned_at: Instant,
  files: HashMap<String, PathBuf>,
}

static AVATARS: LazyLock<Mutex<AvatarIndex>> = LazyLock::new(|| {
  Mutex::new(AvatarIndex {
    scanned_at: Instant::now(),
    files: scan_cached_avatars(),
  })
});

/// A miss normally just means the client has not downloaded that avatar yet,
/// so only walk the caches again once in a while rather than on every refresh.
const AVATAR_RESCAN_AFTER: Duration = Duration::from_secs(5);

/// A cached file that is still on disk.
fn cached_avatar(index: &AvatarIndex, digest: &str) -> Option<PathBuf> {
  index
    .files
    .get(digest)
    .filter(|path| path.is_file())
    .cloned()
}

/// Resolve `client_flag_avatar` to the cached file it describes.
fn local_avatar_file(digest: &str) -> Option<PathBuf> {
  let mut index = AVATARS.lock().ok()?;

  if let Some(path) = cached_avatar(&index, digest) {
    return Some(path);
  }

  if index.scanned_at.elapsed() >= AVATAR_RESCAN_AFTER {
    index.scanned_at = Instant::now();
    index.files = scan_cached_avatars();
    return cached_avatar(&index, digest);
  }

  None
}

/// Apply a `clientvariable` response onto a tracked client.
fn apply_client_variables(client: &mut Ts3Client, params: &[(String, String)]) {
  if let Some(v) = get(params, "client_nickname") {
    client.nickname = v.to_string();
  }
  if let Some(v) = get(params, "client_flag_talking") {
    client.flag_talking = v == "1";
  }
  if let Some(v) = get(params, "client_input_muted") {
    client.input_muted = v == "1";
  }
  if let Some(v) = get(params, "client_output_muted") {
    client.output_muted = v == "1";
  }
  if let Some(v) = get(params, "client_away") {
    client.away = v == "1";
  }
  if let Some(v) = get(params, "client_is_recording") {
    client.is_recording = v == "1";
  }

  // `client_flag_avatar` is the MD5 of the avatar the server holds, and the
  // client keeps a byte-identical copy in its own cache. That local file wins
  // over the myTeamSpeak URL: the URL is a different picture for some clients,
  // while the cached one is what the TS3 client itself puts on screen. The
  // template layer turns a `file://` reference into a loopback URL, because
  // the HTML viewer only ever fetches http and https.
  let local = get(params, "client_flag_avatar")
    .filter(|digest| !digest.is_empty())
    .and_then(local_avatar_file)
    .map(|path| format!("file://{}", path.display()));
  let remote = get(params, "client_myteamspeak_avatar")
    .map(teamspeak_avatar_url)
    .filter(|url| !url.is_empty());

  client.avatar = local.or(remote).unwrap_or_default();
}

/// Build a client from a `clientlist` entry (which carries clid/cid/nickname).
fn client_from_list_entry(params: &[(String, String)]) -> Option<Ts3Client> {
  let id = get(params, "clid")?.to_string();
  let channel_id = get(params, "cid").unwrap_or_default().to_string();
  let nickname = get(params, "client_nickname")
    .unwrap_or("Unknown")
    .to_string();

  Some(Ts3Client {
    id,
    nickname,
    channel_id,
    ..Default::default()
  })
}

/// Locate the TS3 ClientQuery API key: config first, then the client's own
/// `clientquery.ini` so most users never have to set it by hand.
fn find_api_key(configured: Option<String>) -> String {
  if let Some(key) = configured.filter(|k| !k.is_empty()) {
    return key;
  }

  let Some(dir) = config_dir().and_then(|_| dirs_home()) else {
    return String::new();
  };

  let path = dir.join(".ts3client").join("clientquery.ini");
  let Ok(contents) = std::fs::read_to_string(&path) else {
    return String::new();
  };

  contents
    .lines()
    .filter_map(|line| line.split_once('='))
    .find(|(key, _)| key.trim() == "api_key")
    .map(|(_, value)| value.trim().to_string())
    .unwrap_or_default()
}

fn dirs_home() -> Option<std::path::PathBuf> {
  std::env::var_os("HOME").map(std::path::PathBuf::from)
}

fn write_line(stream: &mut TcpStream, line: &str) -> std::io::Result<()> {
  stream.write_all(format!("{line}\n").as_bytes())?;
  stream.flush()
}

fn is_timeout(err: &std::io::Error) -> bool {
  matches!(
    err.kind(),
    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
  )
}

/// Result of a single read attempt.
enum Pumped {
  /// Some bytes were read.
  Data,
  /// Nothing was ready before the read timeout.
  TimedOut,
  /// The peer closed the connection.
  Closed,
}

/// Read whatever the server has sent so far into `buf`.
fn pump(stream: &mut TcpStream, buf: &mut String) -> Pumped {
  let mut temp = [0u8; 8192];

  match stream.read(&mut temp) {
    Ok(0) => Pumped::Closed,
    Ok(n) => {
      buf.push_str(&String::from_utf8_lossy(&temp[..n]));
      Pumped::Data
    }
    Err(ref e) if is_timeout(e) => Pumped::TimedOut,
    Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => Pumped::Closed,
    Err(_) => Pumped::TimedOut,
  }
}

/// Consume complete lines from `buf`.
///
/// ClientQuery separates lines with `\n\r` (LF *then* CR), so each line after
/// the first carries a leading `\r` as well as a trailing one. Trim both ends.
fn drain_lines(buf: &mut String) -> Vec<String> {
  let mut lines = Vec::new();

  while let Some(pos) = buf.find('\n') {
    let line: String = buf.drain(..=pos).collect();
    let line = line.trim_matches(['\r', '\n']).to_string();
    if !line.is_empty() {
      lines.push(line);
    }
  }

  lines
}

/// Wait for the server's unsolicited greeting (no `error id=` terminator).
/// Returns false if the connection closed.
fn read_greeting(stream: &mut TcpStream, buf: &mut String) -> bool {
  loop {
    match pump(stream, buf) {
      Pumped::Data => continue,
      Pumped::TimedOut => break,
      Pumped::Closed => return false,
    }
  }

  drain_lines(buf);
  true
}

/// Send a command and read lines until the terminating `error id=` line,
/// returning the body lines plus the ClientQuery error id.
fn transact_status(
  stream: &mut TcpStream,
  command: &str,
  buf: &mut String,
) -> Result<(Vec<String>, u32), Box<dyn std::error::Error>> {
  write_line(stream, command)?;

  let mut out = Vec::new();
  let mut timeouts = 0;

  loop {
    for line in drain_lines(buf) {
      if line.starts_with("error ") {
        let error_id = line
          .split("error id=")
          .nth(1)
          .and_then(|rest| rest.split(' ').next())
          .and_then(|id| id.parse().ok())
          .unwrap_or(u32::MAX);

        if error_id != 0 {
          warn!("TS3 command `{}` failed: {}", command, line);
        }
        return Ok((out, error_id));
      }

      out.push(line);
    }

    match pump(stream, buf) {
      Pumped::TimedOut => {
        timeouts += 1;
        // The client can be slow to answer; a couple of quiet reads is not a
        // failure, but we stop before spinning forever.
        if timeouts >= 3 {
          return Err("no response from ClientQuery".into());
        }
      }
      Pumped::Closed => return Err("connection closed".into()),
      Pumped::Data => {}
    }
  }
}

/// Send a command and read lines until the terminating `error id=` line,
/// returning everything before it.
fn transact(
  stream: &mut TcpStream,
  command: &str,
  buf: &mut String,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
  transact_status(stream, command, buf).map(|(lines, _)| lines)
}

/// Fetch the given client variables and fold them into the tracked state.
fn refresh_client(
  stream: &mut TcpStream,
  buf: &mut String,
  app: &AppHandle,
  state: &mut Ts3State,
  clid: &str,
) {
  let command = format!("clientvariable clid={clid} {CLIENT_VARIABLES}");
  let Ok(lines) = transact(stream, &command, buf) else {
    return;
  };

  for line in lines {
    let params = parse_params(&line);
    match state.clients.get_mut(clid) {
      Some(client) => apply_client_variables(client, &params),
      None => {
        let mut client = Ts3Client {
          id: clid.to_string(),
          ..Default::default()
        };
        apply_client_variables(&mut client, &params);
        state.clients.insert(clid.to_string(), client);
      }
    }
  }

  state.sync(app);
}

/// Reseed the client list and current channel from the server.
fn seed_state(stream: &mut TcpStream, buf: &mut String, app: &AppHandle, state: &mut Ts3State) {
  if let Ok(lines) = transact(stream, "whoami", buf) {
    for line in &lines {
      let params = parse_params(line);
      if let Some(clid) = get(&params, "clid") {
        state.self_id = clid.to_string();
      }
      if let Some(cid) = get(&params, "cid") {
        state.current_channel = cid.to_string();
      }
    }
  }

  let mut clients = HashMap::new();
  if let Ok(lines) = transact(stream, "clientlist", buf) {
    for line in &lines {
      for entry in line.split('|') {
        let params = parse_params(entry);
        if let Some(client) = client_from_list_entry(&params) {
          clients.insert(client.id.clone(), client);
        }
      }
    }
  }

  state.clients = clients;

  // clientlist omits mute/talk state, so pull it per client.
  let ids: Vec<String> = state.clients.keys().cloned().collect();
  for id in ids {
    let command = format!("clientvariable clid={id} {CLIENT_VARIABLES}");
    if let Ok(lines) = transact(stream, &command, buf) {
      for line in &lines {
        let params = parse_params(line);
        if let Some(client) = state.clients.get_mut(&id) {
          apply_client_variables(client, &params);
        }
      }
    }
  }

  log!(
    "TS3 seeded: self={} channel={} clients=[{}]",
    state.self_id,
    state.current_channel,
    state
      .clients
      .values()
      .map(|c| format!("{}:{} in ch{}", c.id, c.nickname, c.channel_id))
      .collect::<Vec<_>>()
      .join(", ")
  );

  state.sync(app);
}

fn handle_notification(
  line: &str,
  stream: &mut TcpStream,
  buf: &mut String,
  app: &AppHandle,
  state: &mut Ts3State,
) {
  let (event, rest) = match line.split_once(' ') {
    Some((event, rest)) => (event, rest),
    None => (line, ""),
  };
  let params = parse_params(rest);
  log!("TS3 notify: {line}");

  match event {
    "notifytalkstatuschange" => {
      // status: 1 = talking, 2 = stopped, 3 = whispering
      let status = get(&params, "status").unwrap_or("0");
      let Some(clid) = get(&params, "clid") else {
        return;
      };

      if let Some(client) = state.clients.get_mut(clid) {
        client.flag_talking = status == "1";
      }
      state.sync(app);
    }
    "notifycliententerview" => {
      // A client entered our channel. Seed it, then pull full variables.
      let Some(clid) = get(&params, "clid") else {
        return;
      };
      let channel_id = get(&params, "cid").unwrap_or_default().to_string();
      let nickname = get(&params, "client_nickname")
        .unwrap_or("Unknown")
        .to_string();

      state.clients.insert(
        clid.to_string(),
        Ts3Client {
          id: clid.to_string(),
          nickname,
          channel_id,
          ..Default::default()
        },
      );
      state.sync(app);
      refresh_client(stream, buf, app, state, clid);
    }
    "notifyclientleftview" => {
      if let Some(clid) = get(&params, "clid") {
        state.clients.remove(clid);
        state.sync(app);
      }
    }
    "notifyclientmoved" => {
      let Some(clid) = get(&params, "clid") else {
        return;
      };
      let channel_id = get(&params, "ctid").unwrap_or_default().to_string();

      // Our own row has to track the new channel too, otherwise sync()'s
      // channel filter drops us from our own list.
      if clid == state.self_id {
        state.current_channel = channel_id.clone();
      }

      match state.clients.get_mut(clid) {
        Some(client) => client.channel_id = channel_id,
        None => {
          state.clients.insert(
            clid.to_string(),
            Ts3Client {
              id: clid.to_string(),
              channel_id,
              ..Default::default()
            },
          );
        }
      }

      state.sync(app);
    }
    "notifyclientupdated" => {
      if let Some(clid) = get(&params, "clid") {
        refresh_client(stream, buf, app, state, clid);
      }
    }
    "notifytextmessage" => {
      let nickname = get(&params, "invokername").unwrap_or("Unknown").to_string();
      let message = get(&params, "msg").unwrap_or_default().to_string();

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
    _ => {}
  }
}

/// Map an Orbolay control action onto a ClientQuery command.
///
/// Returns any notification lines that arrived interleaved with the command's
/// response, so the caller can still handle them instead of dropping them.
fn handle_ui_message(
  stream: &mut TcpStream,
  msg: &BridgeMessage,
  state: &Ts3State,
  buf: &mut String,
) -> Vec<String> {
  let command = match msg.cmd.as_str() {
    "TOGGLE_MUTE" => {
      let currently_muted = state
        .clients
        .get(&state.self_id)
        .map(|c| c.input_muted)
        .unwrap_or(false);
      format!("clientupdate client_input_muted={}", !currently_muted as u8)
    }
    "TOGGLE_DEAF" => {
      let currently_deafened = state
        .clients
        .get(&state.self_id)
        .map(|c| c.output_muted)
        .unwrap_or(false);
      format!(
        "clientupdate client_output_muted={}",
        !currently_deafened as u8
      )
    }
    // ClientQuery has a dedicated command for this; `clientmove` would need a
    // real channel id and a clid, and would just move us instead of leaving.
    "DISCONNECT" => "disconnect".to_string(),
    _ => return Vec::new(),
  };

  log!("TS3 control: `{}` -> {command}", msg.cmd);

  match transact(stream, &command, buf) {
    Ok(interleaved) => interleaved
      .into_iter()
      .filter(|line| line.starts_with("notify"))
      .collect(),
    Err(e) => {
      error!("TS3 command `{}` failed: {}", msg.cmd, e);
      Vec::new()
    }
  }
}

pub fn create_ts3_client(
  app: AppHandle,
  transport_recv: flume::Receiver<BridgeMessage>,
) -> Result<(), Box<dyn std::error::Error>> {
  let mut reconnect_attempts: u32 = 0;
  let max_reconnect_delay: u64 = 30000;
  let base_reconnect_delay: u64 = 1000;

  loop {
    let (host, port, api_key) = app.read(|state| {
      (
        state
          .config
          .ts3_host
          .clone()
          .unwrap_or_else(|| TS3_HOST_DEFAULT.to_string()),
        state.config.ts3_port.unwrap_or(TS3_PORT_DEFAULT),
        find_api_key(state.config.ts3_api_key.clone()),
      )
    });

    log!("Attempting to connect to TS3 ClientQuery: {host}:{port}");

    match TcpStream::connect((host.as_str(), port)) {
      Ok(mut stream) => {
        log!("TS3 ClientQuery connected");
        reconnect_attempts = 0;

        let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
        let _ = stream.set_write_timeout(Some(Duration::from_millis(2000)));
        let _ = stream.set_nodelay(true);

        let mut buf = String::new();
        let mut state = Ts3State::default();

        let setup: Result<(), Box<dyn std::error::Error>> = (|| {
          // The server greets us unsolicited, then again after auth.
          read_greeting(&mut stream, &mut buf);

          if api_key.is_empty() {
            return Err(
              "no ClientQuery API key found; set one in TS3 under \
               Tools > Options > Client Query"
                .into(),
            );
          }

          transact(&mut stream, &format!("auth apikey={api_key}"), &mut buf)?;

          for event in [
            "notifytalkstatuschange",
            "notifyclientmoved",
            "notifytextmessage",
            "notifycliententerview",
            "notifyclientleftview",
            "notifyclientupdated",
          ] {
            transact(
              &mut stream,
              &format!("clientnotifyregister schandlerid=0 event={event}"),
              &mut buf,
            )?;
          }

          Ok(())
        })();

        if let Err(e) = setup {
          error!("TS3 setup failed: {e}");
          app.notify(Notification {
            title: "TS3 Setup Failed".into(),
            body: e.to_string(),
            icon: String::new(),
            timestamp: Some(chrono::Utc::now().timestamp()),
            timeout_secs: 8,
            actions: None,
            guild_id: None,
            channel_id: None,
            message_id: None,
          });
          app.update(|state| state.voice_users.clear());
          schedule_reconnect(
            &mut reconnect_attempts,
            base_reconnect_delay,
            max_reconnect_delay,
          );
          continue;
        }

        seed_state(&mut stream, &mut buf, &app, &mut state);

        // Leaving the server does not close the ClientQuery connection, so we
        // have to poll `whoami` to notice. It reports error 1794 while we are
        // not connected, and our client id changes on every reconnect.
        let mut connected = true;
        let mut last_poll = Instant::now();

        loop {
          // Apply any control actions the UI asked for.
          while let Ok(msg) = transport_recv.try_recv() {
            let interleaved = handle_ui_message(&mut stream, &msg, &state, &mut buf);
            for line in interleaved {
              handle_notification(&line, &mut stream, &mut buf, &app, &mut state);
            }
          }

          if last_poll.elapsed() >= Duration::from_millis(1500) {
            last_poll = Instant::now();

            match transact_status(&mut stream, "whoami", &mut buf) {
              Ok((interleaved, 0)) => {
                for line in interleaved {
                  if line.starts_with("notify") {
                    handle_notification(&line, &mut stream, &mut buf, &app, &mut state);
                  }
                }

                if !connected {
                  connected = true;
                  log!("TS3 rejoined a server, re-seeding");
                  seed_state(&mut stream, &mut buf, &app, &mut state);
                }
              }
              Ok((_, TS3_ERROR_NOT_CONNECTED)) => {
                if connected {
                  connected = false;
                  log!("TS3 disconnected from server");
                  state = Ts3State::default();
                  app.update(|state| state.voice_users.clear());
                }
              }
              Ok((_, _)) => {}
              Err(e) => {
                warn!("TS3 health check failed: {e}");
                break;
              }
            }
          }

          match pump(&mut stream, &mut buf) {
            Pumped::TimedOut => std::thread::sleep(Duration::from_millis(50)),
            Pumped::Closed => {
              warn!("TS3 ClientQuery closed the connection");
              break;
            }
            Pumped::Data => {}
          }

          for line in drain_lines(&mut buf) {
            if line.starts_with("error ") {
              continue;
            }

            if line.starts_with("notify") {
              handle_notification(&line, &mut stream, &mut buf, &app, &mut state);
            }
          }
        }

        app.update(|state| state.voice_users.clear());
        schedule_reconnect(
          &mut reconnect_attempts,
          base_reconnect_delay,
          max_reconnect_delay,
        );
      }
      Err(e) => {
        error!("TS3 connection failed: {e}");
        app.notify(Notification {
          title: "TS3 Connection Failed".into(),
          body: format!(
            "Failed to connect to TS3 ClientQuery at {host}:{port}: {e}. \
             Enable ClientQuery in TS3 under Tools > Options > Client Query."
          ),
          icon: String::new(),
          timestamp: Some(chrono::Utc::now().timestamp()),
          timeout_secs: 8,
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

fn schedule_reconnect(reconnect_attempts: &mut u32, base: u64, max: u64) {
  let delay = (base * (1u64 << (*reconnect_attempts).min(8))).min(max);
  *reconnect_attempts += 1;
  std::thread::sleep(Duration::from_millis(delay));
}
