//! Serves locally cached avatars over loopback.
//!
//! Freya's HTML viewer only fetches `http` and `https` URLs, so a `file://`
//! avatar is dropped before it ever reaches the decoder. Registering a path
//! here hands back a loopback URL that resolves to the same bytes. Only paths
//! that were registered can be served, so the listener never reads an
//! arbitrary file, and it binds to loopback only.

use std::{
  collections::HashMap,
  io::{Read, Write},
  net::{TcpListener, TcpStream},
  path::PathBuf,
  sync::{LazyLock, Mutex, OnceLock},
  time::Duration,
};

use orbolay_logging::warn;

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::default()));
static PORT: OnceLock<u16> = OnceLock::new();

/// Read timeout so a stalled connection cannot pin a thread forever.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REQUEST: usize = 16 * 1024;

#[derive(Default)]
struct Registry {
  by_token: HashMap<String, PathBuf>,
  by_path: HashMap<PathBuf, String>,
}

/// A loopback URL for a local avatar file. The listener starts on first use,
/// so a run without local avatars never opens a port. An empty string means
/// the server could not start, which the template treats as no image.
pub fn local_avatar_url(path: &str) -> String {
  let path = PathBuf::from(path);
  let port = *PORT.get_or_init(start_server);
  if port == 0 {
    return String::new();
  }

  let token = {
    let mut registry = REGISTRY.lock().unwrap();
    match registry.by_path.get(&path) {
      Some(token) => token.clone(),
      None => {
        let token = registry.by_path.len().to_string();
        registry.by_path.insert(path.clone(), token.clone());
        registry.by_token.insert(token.clone(), path);
        token
      }
    }
  };

  format!("http://127.0.0.1:{port}/avatar/{token}")
}

fn start_server() -> u16 {
  let listener = match TcpListener::bind("127.0.0.1:0") {
    Ok(listener) => listener,
    Err(err) => {
      warn!("Failed to start the local avatar server: {err}");
      return 0;
    }
  };
  let port = listener
    .local_addr()
    .map(|addr| addr.port())
    .unwrap_or_default();

  std::thread::spawn(move || {
    for stream in listener.incoming() {
      let Ok(stream) = stream else { continue };
      std::thread::spawn(move || serve(stream));
    }
  });

  port
}

fn serve(mut stream: TcpStream) {
  let _ = stream.set_read_timeout(Some(READ_TIMEOUT));

  let target = read_target(&mut stream);
  let body = target
    .as_deref()
    .and_then(|target| target.strip_prefix("/avatar/"))
    .and_then(|token| REGISTRY.lock().unwrap().by_token.get(token).cloned())
    .and_then(|path| std::fs::read(path).ok());

  let Some(body) = body else {
    let _ = write!(
      stream,
      "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    return;
  };

  let _ = write!(
    stream,
    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
     Cache-Control: max-age=300\r\nConnection: close\r\n\r\n",
    content_type(&body),
    body.len()
  );
  let _ = stream.write_all(&body);
}

/// The request target, read up to the end of the headers.
fn read_target(stream: &mut TcpStream) -> Option<String> {
  let mut request = Vec::new();
  let mut chunk = [0u8; 1024];

  loop {
    match stream.read(&mut chunk) {
      Ok(0) => break,
      Ok(n) => {
        request.extend_from_slice(&chunk[..n]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") || request.len() >= MAX_REQUEST {
          break;
        }
      }
      Err(_) => return None,
    }
  }

  String::from_utf8_lossy(&request)
    .split_whitespace()
    .nth(1)
    .map(str::to_string)
}

/// TeamSpeak avatars arrive as JPEG, PNG or GIF depending on how they were
/// uploaded, so the bytes decide rather than the file name.
fn content_type(bytes: &[u8]) -> &'static str {
  if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
    "image/png"
  } else if bytes.starts_with(b"GIF8") {
    "image/gif"
  } else {
    "image/jpeg"
  }
}
