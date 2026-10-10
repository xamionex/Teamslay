// Remove some stuff that would break formatting
pub fn strip(text: impl AsRef<str>) -> String {
  text.as_ref().replace("\n", " ")
}

pub fn censor(text: impl AsRef<str>) -> String {
  format!("{}...", text.as_ref().chars().next().unwrap_or('?'))
}

/// TeamSpeak reports avatars as `<mode>,<url>`, for example
/// `2,https://storage.googleapis.com/ts-sys-myts-avatars/...`. Both the TS3
/// ClientQuery property `client_myteamspeak_avatar` and the TS6 WebSocket
/// property `myteamspeakAvatar` use this shape, so they share this parser.
///
/// Anything that is not a plain http(s) URL yields an empty string, which the
/// template layer treats as "no avatar".
pub fn teamspeak_avatar_url(raw: impl AsRef<str>) -> String {
  match raw.as_ref().split_once(',') {
    Some((_, url)) if url.starts_with("http") => url.to_string(),
    _ => String::new(),
  }
}
