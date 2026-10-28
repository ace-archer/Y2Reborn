//! Music server sync: songs and playlists copied from a Jellyfin server, or
//! one that speaks the Subsonic API (Navidrome, Gonic, Airsonic, Ampache and
//! others), to Internal Storage or the SD card. `subsonic` and `jellyfin`
//! speak to the server; this module keeps the account and does the copying.
//!
//! Requests go through `curl` (http or https; a server at home often has no
//! certificate). The address with its sign-in, any header and any body travel
//! on curl's standard input as config lines (`--config -`), never on its
//! command line, so no secret shows in the process list. The password is used
//! once at sign-in and never stored: what is kept (a Subsonic token and salt,
//! an OpenSubsonic API key or a Jellyfin access token) lives only in
//! `/data/reborn/state/music_server.json` (0600). Nothing here logs a secret.
//!
//! Everything synced lives in one `Music Server` folder on the chosen storage:
//! `Artist/Album/NN Title.ext` and `Playlists/<name>.m3u8`, with
//! `.sync.json` listing what the sync put there. Only files in that list are
//! ever replaced or removed; the server is the source, so a synced playlist
//! changed on the player is written over by the next sync.
use crate::{jellyfin, subsonic};
use reborn_core::music_server::{SyncQuality, SyncScope};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The folder on the chosen storage that holds everything synced.
pub const FOLDER: &str = "Music Server";
const MANIFEST: &str = ".sync.json";
const PLAYLISTS: &str = "Playlists";
/// Free space a sync leaves on the storage.
pub const RESERVE: u64 = 256 * 1024 * 1024;
/// At most this many songs are listed.
pub(crate) const MAX_SONGS: usize = 200_000;
/// The manifest is written at most this often during a sync (and when it
/// stops), so an interruption keeps what was copied without rewriting a
/// large library's list after every few songs.
const SAVE_EVERY: Duration = Duration::from_secs(60);
/// Longest name of one folder or file (FAT32 and exFAT allow 255).
const MAX_NAME_CHARS: usize = 80;
const CURL: &str = "/usr/bin/curl";

// --------------------------------------------------------------- account

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Account {
    /// `http(s)://host[:port][/path]`, no trailing slash.
    pub server: String,
    pub username: String,
    pub auth: Auth,
}

/// What signs the requests: never the password itself.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Auth {
    /// Subsonic's token sign-in: `md5(password + salt)` signs every request.
    SubsonicToken { salt: String, token: String },
    /// An OpenSubsonic API key, made on the server for this player (for a
    /// server that refuses token sign-in).
    SubsonicKey { key: String },
    /// A Jellyfin access token for this player (`device_id`).
    Jellyfin {
        user_id: String,
        device_id: String,
        token: String,
    },
}

/// Never the secret, in a log or a test failure.
impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::SubsonicToken { .. } => "SubsonicToken",
            Self::SubsonicKey { .. } => "SubsonicKey",
            Self::Jellyfin { .. } => "Jellyfin",
        })
    }
}
impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Account {{ server: {:?}, username: {:?}, kind: {:?}, .. }}",
            self.server,
            self.username,
            self.kind()
        )
    }
}
impl Account {
    /// A Subsonic token sign-in for `server` (checked with `server_address`);
    /// the password is used for the token here and nowhere else.
    pub fn subsonic_token(server: String, username: String, password: &str) -> Self {
        let salt = format!("{:016x}", crate::random_u64());
        let token = crate::lastfm::hex(&crate::lastfm::md5(format!("{password}{salt}").as_bytes()));
        Self {
            server,
            username,
            auth: Auth::SubsonicToken { salt, token },
        }
    }
    /// The server's host as the page shows it ("music.example.com").
    pub fn host(&self) -> &str {
        let rest = self
            .server
            .split_once("://")
            .map_or(self.server.as_str(), |(_, r)| r);
        rest.split('/').next().unwrap_or(rest)
    }
    /// Which API the server speaks, for the log.
    pub fn kind(&self) -> &'static str {
        match self.auth {
            Auth::SubsonicToken { .. } => "subsonic",
            Auth::SubsonicKey { .. } => "subsonic_api_key",
            Auth::Jellyfin { .. } => "jellyfin",
        }
    }
    fn complete(&self) -> bool {
        let filled = match &self.auth {
            Auth::SubsonicToken { salt, token } => !salt.is_empty() && !token.is_empty(),
            Auth::SubsonicKey { key } => !key.is_empty(),
            Auth::Jellyfin { user_id, token, .. } => !user_id.is_empty() && !token.is_empty(),
        };
        filled && server_address(&self.server).is_some() && !self.username.is_empty()
    }
}

/// The address typed on the player as the server's base address: http or
/// https (https when none is given), no spaces, quotes or trailing slash.
pub fn server_address(text: &str) -> Option<String> {
    let text = text.trim();
    let full = if text.contains("://") {
        text.to_owned()
    } else {
        format!("https://{text}")
    };
    let full = full.trim_end_matches('/').to_owned();
    let (scheme, rest) = full.split_once("://")?;
    let ok = matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https")
        && !rest.split('/').next().unwrap_or("").is_empty()
        && rest
            .chars()
            .all(|c| c.is_ascii_graphic() && !matches!(c, '"' | '\\' | '?' | '#'));
    ok.then_some(full)
}

pub fn account_path(root: &Path) -> PathBuf {
    root.join("state/music_server.json")
}
pub fn load_account(path: &Path) -> Option<Account> {
    let a: Account = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    a.complete().then_some(a)
}
pub fn save_account(path: &Path, account: &Account) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec(account).map_err(io::Error::other)?)?;
        file.sync_all()?;
    }
    fs::rename(tmp, path)
}
pub fn forget_account(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

// --------------------------------------------------------------- errors

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// No connection, DNS, TLS or a timeout: try again later.
    Offline(String),
    /// Wrong username, password or API key.
    WrongLogin,
    /// A Subsonic server that takes neither token sign-in nor API keys.
    TokenRefused,
    /// A Jellyfin account its administrator keeps from downloading.
    NoDownloads,
    /// The address answers, but not as a Jellyfin or Subsonic server.
    NotAServer,
    /// Anything else the server refuses: its code (Subsonic's, or the HTTP
    /// status) and message.
    Refused(i64, String),
    /// Writing on the player failed (a card removed, a read-only storage).
    Storage(String),
    Cancelled,
}
impl Error {
    pub fn retry_later(&self) -> bool {
        matches!(self, Self::Offline(_))
    }
    /// What went wrong underneath, for the log (no secrets: curl exit
    /// codes, I/O errors, the server's own message).
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Offline(d) | Self::Storage(d) | Self::Refused(_, d) => Some(d),
            _ => None,
        }
    }
    /// For the listener, in Reborn's words.
    pub fn message(&self) -> String {
        match self {
            Self::Offline(_) => "The server can't be reached".into(),
            Self::WrongLogin => "Wrong username or password".into(),
            Self::TokenRefused => "This server doesn't accept this sign-in".into(),
            Self::NoDownloads => "This account isn't allowed to download".into(),
            Self::NotAServer => "That address isn't a Jellyfin or Subsonic server".into(),
            Self::Refused(code, _) => format!("The server refused the request ({code})"),
            Self::Storage(_) => "Couldn't write the music to storage".into(),
            Self::Cancelled => "Sync stopped".into(),
        }
    }
}
pub(crate) fn storage(e: io::Error) -> Error {
    Error::Storage(e.to_string())
}

// ----------------------------------------------------------------- songs

/// One song as the server lists it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Song {
    pub id: String,
    pub title: String,
    pub album: String,
    pub artist: String,
    /// The album's artist, for the folder (falls back to `artist`).
    pub album_artist: String,
    pub track: u32,
    pub disc: u32,
    pub suffix: String,
    pub size: u64,
    pub duration_s: u32,
    pub bitrate_kbps: u32,
}
impl Song {
    /// The MP3 rate the server makes this song at, or `None` for the
    /// original file (asked for, or an MP3 already no larger).
    pub fn transcode(&self, quality: SyncQuality) -> Option<u32> {
        let kbps = quality.mp3_kbps()?;
        (!(self.suffix == "mp3" && self.bitrate_kbps > 0 && self.bitrate_kbps <= kbps))
            .then_some(kbps)
    }
    /// How many bytes the file takes, as far as the listing tells.
    pub fn expected_bytes(&self, quality: SyncQuality) -> u64 {
        match self.transcode(quality) {
            Some(kbps) => u64::from(self.duration_s) * u64::from(kbps) * 125,
            None => self.size,
        }
    }
    /// `Artist/Album/NN Title.ext` inside the sync folder.
    pub fn relative_path(&self, quality: SyncQuality) -> PathBuf {
        let artist = if self.album_artist.is_empty() {
            &self.artist
        } else {
            &self.album_artist
        };
        let number = match (self.disc, self.track) {
            (d, t) if d > 1 && t > 0 => format!("{d}-{t:02} "),
            (_, t) if t > 0 => format!("{t:02} "),
            _ => String::new(),
        };
        let suffix = match self.transcode(quality) {
            Some(_) => "mp3",
            None if self.suffix.is_empty() => "bin",
            None => &self.suffix,
        };
        let file = format!(
            "{}.{}",
            name_part(&format!("{number}{}", self.title)),
            suffix
        );
        PathBuf::from(name_part(artist))
            .join(name_part(&self.album))
            .join(file)
    }
}

/// A folder or file name every storage takes: no characters FAT forbids, no
/// leading dot (hidden), no trailing dot or space, at most `MAX_NAME_CHARS`.
pub fn name_part(text: &str) -> String {
    let mapped: String = text
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .take(MAX_NAME_CHARS)
        .collect();
    let trimmed = mapped.trim().trim_end_matches(['.', ' ']);
    let trimmed = trimmed
        .strip_prefix('.')
        .map_or(trimmed.to_owned(), |rest| format!("_{rest}"));
    if trimmed.is_empty() {
        "Unknown".into()
    } else {
        trimmed
    }
}

/// A playlist on the server, with its songs in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePlaylist {
    pub id: String,
    pub name: String,
    pub songs: Vec<Song>,
}

/// A JSON list that a server may also send as one object, or leave out.
pub(crate) fn list(v: Option<&Value>) -> Vec<&Value> {
    match v {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(item @ Value::Object(_)) => vec![item],
        _ => vec![],
    }
}

// ------------------------------------------------------------- transport

/// One request for curl: the address, any headers and any body.
#[derive(Debug, Clone, Default)]
pub(crate) struct Request {
    pub url: String,
    pub headers: Vec<String>,
    pub body: Option<String>,
}
impl Request {
    pub fn get(url: String) -> Self {
        Self {
            url,
            ..Self::default()
        }
    }
    /// curl's config lines: every value quoted, so nothing in it is read as
    /// another option.
    fn config(&self) -> String {
        let quoted =
            |text: &str| format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""));
        let mut lines = format!("url = {}\n", quoted(&self.url));
        for header in &self.headers {
            lines.push_str(&format!("header = {}\n", quoted(header)));
        }
        if let Some(body) = &self.body {
            lines.push_str(&format!("data-raw = {}\n", quoted(body)));
        }
        lines
    }
}

/// The curl program (tests use a stand-in).
#[derive(Debug, Clone)]
pub(crate) struct Curl {
    pub program: String,
}
impl Curl {
    pub fn new() -> Self {
        Self {
            program: std::env::var("REBORN_CURL").unwrap_or_else(|_| CURL.into()),
        }
    }
    /// curl for http(s) only, redirects followed, the request on stdin and
    /// the HTTP status on the last line of its output.
    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args([
                "--silent",
                "--show-error",
                "--location",
                "--max-redirs",
                "5",
            ])
            .args(["--proto", "=http,https", "--proto-redir", "=http,https"])
            .args(["--connect-timeout", "10"])
            .args([
                "--user-agent",
                concat!("Y2Reborn/", env!("CARGO_PKG_VERSION")),
            ])
            .args(["--write-out", "\\n%{http_code}"])
            .args(["--config", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
    }
    fn send(child: &mut Child, request: &Request) -> Result<(), Error> {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Offline("no stdin".into()))?;
        stdin
            .write_all(request.config().as_bytes())
            .map_err(|e| Error::Offline(e.to_string()))
    }
    /// The HTTP status and the reply. No status (no connection, DNS, TLS, a
    /// timeout) is `Offline`.
    pub fn fetch(&self, request: &Request) -> Result<(u16, Vec<u8>), Error> {
        let mut child = self
            .command()
            .args(["--max-time", "60"])
            .spawn()
            .map_err(|e| Error::Offline(e.to_string()))?;
        Self::send(&mut child, request)?;
        let mut reply = vec![];
        if let Some(mut out) = child.stdout.take() {
            let _ = out.by_ref().take(32 * 1024 * 1024).read_to_end(&mut reply);
        }
        let status = child.wait().map_err(|e| Error::Offline(e.to_string()))?;
        match http_status(&mut reply) {
            0 => Err(Error::Offline(format!(
                "curl {}",
                status.code().unwrap_or(-1)
            ))),
            code => Ok((code, reply)),
        }
    }
    /// Download into `to` (written whole or not at all is the caller's
    /// rename) and answer the HTTP status. Stops at once when `cancel` is
    /// set.
    pub fn download(
        &self,
        request: &Request,
        to: &Path,
        cancel: &AtomicBool,
    ) -> Result<u16, Error> {
        let mut child = self
            .command()
            // A stalled transfer (under 1 kB/s for a minute) is given up.
            .args(["--speed-limit", "1024", "--speed-time", "60"])
            .arg("--output")
            .arg(to)
            .spawn()
            .map_err(|e| Error::Offline(e.to_string()))?;
        Self::send(&mut child, request)?;
        let status = loop {
            if cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Cancelled);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => return Err(Error::Offline(e.to_string())),
            }
        };
        let mut said = vec![];
        if let Some(mut out) = child.stdout.take() {
            let _ = out.by_ref().take(64).read_to_end(&mut said);
        }
        match status.code() {
            Some(0) => {}
            Some(23) => return Err(Error::Storage("curl could not write".into())),
            code => return Err(Error::Offline(format!("curl {}", code.unwrap_or(-1)))),
        }
        Ok(http_status(&mut said))
    }
}

/// Take the status curl wrote on the last line (`--write-out`) off the
/// reply; 0 when there is none.
fn http_status(reply: &mut Vec<u8>) -> u16 {
    let at = reply.iter().rposition(|&b| b == b'\n');
    let code = std::str::from_utf8(&reply[at.map_or(0, |i| i + 1)..])
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    if code != 0 {
        reply.truncate(at.unwrap_or(0));
    }
    code
}

// ---------------------------------------------------------------- client

/// A signed-in server, whichever API it speaks.
#[derive(Debug, Clone)]
pub enum Client {
    Subsonic(subsonic::Client),
    Jellyfin(jellyfin::Client),
}
impl Client {
    pub fn new(account: Account) -> Self {
        Self::with(account, Curl::new())
    }
    pub(crate) fn with(account: Account, curl: Curl) -> Self {
        match account.auth {
            Auth::Jellyfin { .. } => Self::Jellyfin(jellyfin::Client { account, curl }),
            Auth::SubsonicToken { .. } | Auth::SubsonicKey { .. } => {
                Self::Subsonic(subsonic::Client { account, curl })
            }
        }
    }
    pub fn account(&self) -> &Account {
        match self {
            Self::Subsonic(c) => &c.account,
            Self::Jellyfin(c) => &c.account,
        }
    }
    /// The sign-in still works.
    pub fn ping(&self) -> Result<(), Error> {
        match self {
            Self::Subsonic(c) => c.ping(),
            Self::Jellyfin(c) => c.ping(),
        }
    }
    /// Every song in the library.
    pub fn songs(&self, cancel: &AtomicBool) -> Result<Vec<Song>, Error> {
        match self {
            Self::Subsonic(c) => c.songs(cancel),
            Self::Jellyfin(c) => c.songs(cancel),
        }
    }
    /// Every playlist the account sees, with its songs.
    pub fn playlists(&self, cancel: &AtomicBool) -> Result<Vec<RemotePlaylist>, Error> {
        match self {
            Self::Subsonic(c) => c.playlists(cancel),
            Self::Jellyfin(c) => c.playlists(cancel),
        }
    }
    /// One song into `to`, as `quality` asks.
    pub fn download(
        &self,
        song: &Song,
        quality: SyncQuality,
        to: &Path,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        match self {
            Self::Subsonic(c) => c.download(song, quality, to, cancel),
            Self::Jellyfin(c) => c.download(song, quality, to, cancel),
        }
    }
}

/// Sign in to `server` (checked with `server_address`): a Jellyfin server
/// says so at `/System/Info/Public`; anything else is asked as a Subsonic
/// server. The password is used here and never kept.
pub fn sign_in(server: String, username: String, password: &str) -> Result<Account, Error> {
    sign_in_with(&Curl::new(), server, username, password)
}
pub(crate) fn sign_in_with(
    curl: &Curl,
    server: String,
    username: String,
    password: &str,
) -> Result<Account, Error> {
    if jellyfin::is_jellyfin(curl, &server)? {
        jellyfin::sign_in(curl, server, username, password)
    } else {
        subsonic::sign_in(curl, server, username, password)
    }
}

// -------------------------------------------------------------- manifest

/// One song the sync put on the player.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kept {
    /// Inside the sync folder, `/`-separated.
    pub path: String,
    /// The server's size of the original: a changed file is copied again.
    pub size: u64,
    pub quality: SyncQuality,
}

/// What the sync put in its folder (`Music Server/.sync.json`). It lives with
/// the music, so another card brings its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub server: String,
    pub username: String,
    /// Unix time of the last complete sync; 0 never.
    #[serde(default)]
    pub synced_at: i64,
    /// By song id.
    #[serde(default)]
    pub songs: BTreeMap<String, Kept>,
    /// Playlist file inside the sync folder, by playlist id.
    #[serde(default)]
    pub playlists: BTreeMap<String, String>,
}
pub fn folder(root: &Path) -> PathBuf {
    root.join(FOLDER)
}
pub fn load_manifest(root: &Path) -> Manifest {
    fs::read(folder(root).join(MANIFEST))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}
pub(crate) fn save_manifest(root: &Path, manifest: &Manifest) -> Result<(), Error> {
    let bytes = serde_json::to_vec(manifest).map_err(|e| Error::Storage(e.to_string()))?;
    reborn_core::atomic_write(&folder(root).join(MANIFEST), &bytes).map_err(storage)
}

// ------------------------------------------------------------------ sync

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    pub scope: SyncScope,
    pub quality: SyncQuality,
    pub now_unix: i64,
    /// Free space left on the storage (`RESERVE`; tests choose).
    pub reserve: u64,
}

/// Where a running sync is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Progress {
    /// Songs copied or failed of `total` to copy.
    pub done: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    pub downloaded: usize,
    pub removed: usize,
    /// Songs on the player after the sync.
    pub songs: usize,
    pub playlists: usize,
    /// Songs the server would not send.
    pub failed: usize,
    /// The storage filled up: some songs were left out.
    pub full: bool,
}

/// Bring `root/Music Server` in line with the server: copy what is missing or
/// changed, remove what the server no longer has (or `scope` no longer
/// wants), then write the playlists. Each song is written whole or not at
/// all; an interruption keeps what was copied.
pub fn sync(
    client: &Client,
    root: &Path,
    options: &Options,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<Summary, Error> {
    let dir = folder(root);
    fs::create_dir_all(&dir).map_err(storage)?;
    client.ping()?;
    let playlists = client.playlists(cancel)?;
    let wanted: Vec<Song> = match options.scope {
        SyncScope::All => client.songs(cancel)?,
        SyncScope::Playlists => {
            let mut seen = HashSet::new();
            playlists
                .iter()
                .flat_map(|p| &p.songs)
                .filter(|s| seen.insert(s.id.clone()))
                .cloned()
                .collect()
        }
    };
    let mut manifest = load_manifest(root);
    let account = client.account();
    let mut removed = 0;
    if manifest.server != account.server || manifest.username != account.username {
        // Another server's songs: none of them is wanted.
        removed = remove_songs(&dir, &mut manifest, |_| true);
        manifest.server = account.server.clone();
        manifest.username = account.username.clone();
        manifest.synced_at = 0;
    }
    // Keep what is there unchanged; everything else is copied again.
    let unchanged = |s: &Song, k: &Kept| {
        k.size == s.size && k.quality == options.quality && dir.join(&k.path).is_file()
    };
    let wanted_ids: HashMap<&str, &Song> = wanted.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut summary = Summary {
        removed: removed
            + remove_songs(&dir, &mut manifest, |(id, k)| {
                wanted_ids.get(id.as_str()).is_none_or(|s| !unchanged(s, k))
            }),
        ..Summary::default()
    };
    let mut used: HashSet<String> = manifest
        .songs
        .values()
        .map(|k| k.path.to_lowercase())
        .collect();
    let copy: Vec<(&Song, String)> = wanted
        .iter()
        .filter(|s| !manifest.songs.contains_key(&s.id))
        .map(|s| {
            let path = unique_path(s, options.quality, &mut used);
            (s, path)
        })
        .collect();
    let mut saved = Instant::now();
    let mut at = Progress {
        done: 0,
        total: copy.len(),
    };
    progress(at);
    // Every way out of the copying keeps the list of what was copied
    // (best effort: the error that stopped it is the one to report).
    let copied = (|| {
        for (song, path) in &copy {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let free = crate::free_bytes(&dir).map_err(Error::Storage)?;
            if free
                < song
                    .expected_bytes(options.quality)
                    .saturating_add(options.reserve)
            {
                summary.full = true;
                break;
            }
            let target = dir.join(path);
            let parent = target.parent().unwrap_or(&dir);
            fs::create_dir_all(parent).map_err(storage)?;
            let part = parent.join(format!(
                ".{}.part",
                target.file_name().unwrap_or_default().to_string_lossy()
            ));
            match client.download(song, options.quality, &part, cancel) {
                Ok(()) => {
                    fs::rename(&part, &target).map_err(storage)?;
                    manifest.songs.insert(
                        song.id.clone(),
                        Kept {
                            path: path.clone(),
                            size: song.size,
                            quality: options.quality,
                        },
                    );
                    summary.downloaded += 1;
                }
                Err(Error::Refused(..) | Error::NotAServer) => {
                    let _ = fs::remove_file(&part);
                    summary.failed += 1;
                }
                Err(e) => {
                    let _ = fs::remove_file(&part);
                    return Err(e);
                }
            }
            at.done += 1;
            progress(at);
            if saved.elapsed() >= SAVE_EVERY {
                save_manifest(root, &manifest)?;
                saved = Instant::now();
            }
        }
        Ok(())
    })();
    if let Err(e) = copied {
        let _ = save_manifest(root, &manifest);
        return Err(e);
    }
    summary.playlists = write_playlists(&dir, &mut manifest, &playlists)?;
    remove_empty_folders(&dir);
    summary.songs = manifest.songs.len();
    manifest.synced_at = options.now_unix;
    save_manifest(root, &manifest)?;
    Ok(summary)
}

/// Remove the kept songs `drop` picks (a file already gone is fine).
fn remove_songs(
    dir: &Path,
    manifest: &mut Manifest,
    mut drop: impl FnMut((&String, &Kept)) -> bool,
) -> usize {
    let gone: Vec<String> = manifest
        .songs
        .iter()
        .filter(|&entry| drop(entry))
        .map(|(id, _)| id.clone())
        .collect();
    for id in &gone {
        if let Some(k) = manifest.songs.remove(id) {
            let _ = fs::remove_file(dir.join(&k.path));
        }
    }
    gone.len()
}

/// The song's path, made unique (case aside, as FAT compares) with its id.
fn unique_path(song: &Song, quality: SyncQuality, used: &mut HashSet<String>) -> String {
    let path = song.relative_path(quality);
    let mut text = path.to_string_lossy().into_owned();
    if !used.insert(text.to_lowercase()) {
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        let ext = path.extension().unwrap_or_default().to_string_lossy();
        let id: String = song
            .id
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(8)
            .collect();
        text = path
            .with_file_name(format!("{stem} ({id}).{ext}"))
            .to_string_lossy()
            .into_owned();
        used.insert(text.to_lowercase());
    }
    text
}

/// Each server playlist as `Playlists/<name>.m3u8` with the songs the player
/// has, paths relative to it. A file is rewritten only when it changes (the
/// quick scan reads only changed folders). Returns how many there are.
fn write_playlists(
    dir: &Path,
    manifest: &mut Manifest,
    playlists: &[RemotePlaylist],
) -> Result<usize, Error> {
    let folder = dir.join(PLAYLISTS);
    let mut written = BTreeMap::new();
    let mut used = HashSet::new();
    // The player's own Favorites is `Playlists/Favorites` on any storage
    // (`playlists::FAVORITES`): a server playlist of that name keeps apart.
    used.insert(crate::playlists::FAVORITES.to_lowercase());
    for p in playlists {
        let mut name = name_part(&p.name);
        if name.eq_ignore_ascii_case(crate::playlists::FAVORITES) {
            name = format!("{name} (Server)");
        }
        if !used.insert(name.to_lowercase()) {
            name = format!("{name} ({})", name_part(&p.id));
            used.insert(name.to_lowercase());
        }
        let mut text = String::from("#EXTM3U\n");
        // One line each: a line break in a tag would end the entry.
        let line = |t: &str| t.replace(['\r', '\n'], " ");
        for s in &p.songs {
            if let Some(k) = manifest.songs.get(&s.id) {
                text.push_str(&format!(
                    "#EXTINF:{},{} - {}\n../{}\n",
                    s.duration_s,
                    line(&s.artist),
                    line(&s.title),
                    k.path
                ));
            }
        }
        let relative = format!("{PLAYLISTS}/{name}.m3u8");
        let file = dir.join(&relative);
        if fs::read(&file).ok().as_deref() != Some(text.as_bytes()) {
            fs::create_dir_all(&folder).map_err(storage)?;
            reborn_core::atomic_write(&file, text.as_bytes()).map_err(storage)?;
        }
        written.insert(p.id.clone(), relative);
    }
    for (id, relative) in &manifest.playlists {
        if written.get(id) != Some(relative) && !written.values().any(|r| r == relative) {
            let _ = fs::remove_file(dir.join(relative));
        }
    }
    manifest.playlists = written;
    Ok(manifest.playlists.len())
}

/// Folders emptied by removed songs go too (the sync folder itself stays).
fn remove_empty_folders(dir: &Path) {
    fn walk(path: &Path) -> bool {
        let Ok(entries) = fs::read_dir(path) else {
            return false;
        };
        let mut empty = true;
        for entry in entries.flatten() {
            let p = entry.path();
            if !(p.is_dir() && walk(&p) && fs::remove_dir(&p).is_ok()) {
                empty = false;
            }
        }
        empty
    }
    walk(dir);
}

/// What the tests of the sync and of both APIs share: a scratch folder, a
/// song, and a stand-in curl.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    pub fn dir(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("reborn-music-server-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    pub fn song(id: &str, title: &str, size: u64) -> Song {
        Song {
            id: id.into(),
            title: title.into(),
            album: "Mask".into(),
            artist: "Bauhaus".into(),
            track: 1,
            suffix: "flac".into(),
            size,
            duration_s: 200,
            bitrate_kbps: 900,
            ..Default::default()
        }
    }

    /// A stand-in curl that records its command lines (`root/argv`) and the
    /// config it was sent (`root/requests`), then answers with the shell
    /// `case` arms given: they match the config (`$cfg`), may write the
    /// `--output` file (`$out`) and set the HTTP status (`$code`, else 200).
    pub fn curl(root: &Path, arms: &str) -> Curl {
        let script = root.join("curl");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$@\" >> {r}/argv\nout=''\n\
                 while [ $# -gt 0 ]; do [ \"$1\" = --output ] && out=\"$2\"; shift; done\n\
                 cfg=$(cat)\necho \"$cfg\" >> {r}/requests\ncode=200\n\
                 case \"$cfg\" in\n{arms}\nesac\nprintf '\\n%s' \"$code\"\n",
                r = root.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        // A test thread forking just now holds the script open for writing
        // until its child execs ("text file busy"): wait until it runs.
        for _ in 0..100 {
            let ran = Command::new(&script)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .status();
            if !matches!(&ran, Err(e) if e.raw_os_error() == Some(libc::ETXTBSY)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = fs::remove_file(root.join("argv"));
        let _ = fs::remove_file(root.join("requests"));
        Curl {
            program: script.display().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{dir, song};
    use super::*;
    use crate::subsonic::fake;

    #[test]
    fn addresses_and_account_files() {
        assert_eq!(
            server_address(" music.example.com/ ").as_deref(),
            Some("https://music.example.com")
        );
        assert_eq!(
            server_address("http://192.168.1.5:4533").as_deref(),
            Some("http://192.168.1.5:4533")
        );
        for bad in [
            "",
            "ftp://x",
            "https://",
            "https://a b",
            "https://x\"y",
            "https://x?y",
        ] {
            assert_eq!(server_address(bad), None, "{bad:?}");
        }
        let root = dir("account");
        let path = account_path(&root);
        let a = Account::subsonic_token(
            "https://music.example.com/nd".into(),
            "luca".into(),
            "hunter2",
        );
        let Auth::SubsonicToken { salt, token } = &a.auth else {
            unreachable!()
        };
        assert_eq!(
            *token,
            crate::lastfm::hex(&crate::lastfm::md5(format!("hunter2{salt}").as_bytes()))
        );
        assert_eq!(a.host(), "music.example.com");
        assert!(!format!("{a:?}").contains(token.as_str()));
        save_account(&path, &a).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("hunter2"), "the password is never stored");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(load_account(&path), Some(a.clone()));
        // An account file missing its secret is no sign-in.
        let empty = Account {
            auth: Auth::SubsonicKey { key: String::new() },
            ..a
        };
        save_account(&path, &empty).unwrap();
        assert_eq!(load_account(&path), None);
        forget_account(&path).unwrap();
        forget_account(&path).unwrap();
        assert_eq!(load_account(&path), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn paths_are_safe_on_every_storage() {
        assert_eq!(name_part("AC/DC: Live?"), "AC_DC_ Live_");
        assert_eq!(name_part(".hidden"), "_hidden");
        assert_eq!(name_part("  trailing. . "), "trailing");
        assert_eq!(name_part(""), "Unknown");
        assert_eq!(name_part(&"x".repeat(300)).chars().count(), MAX_NAME_CHARS);
        let mut s = song("1", "Hollow Hills", 1);
        s.disc = 2;
        s.track = 3;
        s.album_artist = "Bauhaus & Friends".into();
        assert_eq!(
            s.relative_path(SyncQuality::Original),
            PathBuf::from("Bauhaus & Friends/Mask/2-03 Hollow Hills.flac")
        );
        assert_eq!(s.transcode(SyncQuality::Mp3High), Some(320));
        assert!(s
            .relative_path(SyncQuality::Mp3High)
            .to_string_lossy()
            .ends_with(".mp3"));
        assert_eq!(s.expected_bytes(SyncQuality::Mp3High), 200 * 320 * 125);
        // An MP3 no larger than asked is kept as it is.
        s.suffix = "mp3".into();
        s.bitrate_kbps = 192;
        assert_eq!(s.transcode(SyncQuality::Mp3Medium), None);
        let mut used = HashSet::new();
        let a = unique_path(
            &song("aaaa1111zz", "Same", 1),
            SyncQuality::Original,
            &mut used,
        );
        let b = unique_path(
            &song("bbbb2222zz", "SAME", 1),
            SyncQuality::Original,
            &mut used,
        );
        assert_eq!(a, "Bauhaus/Mask/01 Same.flac");
        assert_eq!(b, "Bauhaus/Mask/01 SAME (bbbb2222).flac");
    }

    fn options(scope: SyncScope) -> Options {
        Options {
            scope,
            quality: SyncQuality::Original,
            now_unix: 1_800_000_000,
            reserve: 0,
        }
    }

    #[test]
    fn sync_copies_keeps_and_removes_only_its_own_files() {
        let root = dir("sync");
        let music = root.join("music");
        let songs = vec![
            song("s1", "Hollow Hills", 10),
            song("s2", "Kick in the Eye", 20),
            song("bad", "Missing", 30),
        ];
        let client = fake::server(&root, &songs, &["s2", "s1"]);
        let never = AtomicBool::new(false);
        let mut seen = vec![];
        let summary = sync(
            &client,
            &music,
            &options(SyncScope::All),
            &never,
            &mut |p| seen.push(p),
        )
        .unwrap();
        assert_eq!(
            summary,
            Summary {
                downloaded: 2,
                removed: 0,
                songs: 2,
                playlists: 2,
                failed: 1,
                full: false
            }
        );
        assert_eq!(seen.last(), Some(&Progress { done: 3, total: 3 }));
        let sync_dir = folder(&music);
        assert!(sync_dir.join("Bauhaus/Mask/01 Hollow Hills.flac").is_file());
        assert!(!sync_dir.join("Bauhaus/Mask/01 Missing.flac").exists());
        assert!(!sync_dir.join("Bauhaus/Mask/.01 Missing.flac.part").exists());
        assert_eq!(
            fs::read_to_string(sync_dir.join("Playlists/Road_Trip.m3u8")).unwrap(),
            "#EXTM3U\n#EXTINF:200,Bauhaus - Kick in the Eye\n../Bauhaus/Mask/01 Kick in the Eye.flac\n\
             #EXTINF:200,Bauhaus - Hollow Hills\n../Bauhaus/Mask/01 Hollow Hills.flac\n"
        );
        // A server playlist named Favorites is not taken for the player's own.
        assert!(sync_dir.join("Playlists/favorites (Server).m3u8").is_file());
        assert!(!sync_dir.join("Playlists/favorites.m3u8").exists());
        // No secret on any command line; the token goes on stdin.
        let Auth::SubsonicToken { token, .. } = &client.account().auth else {
            unreachable!()
        };
        let argv = fs::read_to_string(root.join("argv")).unwrap();
        assert!(!argv.contains(token.as_str()) && argv.contains("--config -"));
        assert!(fs::read_to_string(root.join("requests"))
            .unwrap()
            .contains(&format!("t={token}")));
        assert_eq!(load_manifest(&music).synced_at, 1_800_000_000);

        // Again: nothing to copy. A file of the listener's own stays.
        let own = sync_dir.join("Bauhaus/Mask/notes.txt");
        fs::write(&own, "mine").unwrap();
        fs::remove_file(root.join("requests")).unwrap();
        let again = sync(
            &client,
            &music,
            &options(SyncScope::All),
            &never,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!((again.downloaded, again.removed, again.failed), (0, 0, 1));
        // Only the song the server refused is asked for again.
        let requests = fs::read_to_string(root.join("requests")).unwrap();
        let downloads: Vec<&str> = requests
            .lines()
            .filter(|l| l.contains("rest/download"))
            .collect();
        assert!(downloads.len() == 1 && downloads[0].contains("id=bad"));

        // Playlists only: s1 and s2 stay; a song changed on the server is
        // copied again; one gone from the server is removed with its folder.
        let mut changed = songs.clone();
        changed[1].size = 21;
        changed[0].album = "Elsewhere".into();
        let client = fake::server(&root, &changed, &["s2"]);
        let third = sync(
            &client,
            &music,
            &options(SyncScope::Playlists),
            &never,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!((third.downloaded, third.removed, third.songs), (1, 2, 1));
        assert!(!sync_dir.join("Bauhaus/Mask/01 Hollow Hills.flac").exists());
        assert!(sync_dir
            .join("Bauhaus/Mask/01 Kick in the Eye.flac")
            .is_file());
        assert!(own.is_file(), "files the sync did not put there stay");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sync_stops_for_space_cancel_and_another_account() {
        let root = dir("limits");
        let music = root.join("music");
        let songs = vec![song("s1", "One", 10), song("s2", "Two", 20)];
        let client = fake::server(&root, &songs, &[]);
        let never = AtomicBool::new(false);
        let full = sync(
            &client,
            &music,
            &Options {
                reserve: u64::MAX / 2,
                ..options(SyncScope::All)
            },
            &never,
            &mut |_| {},
        )
        .unwrap();
        assert!(full.full && full.downloaded == 0);
        let stop = AtomicBool::new(true);
        assert_eq!(
            sync(
                &client,
                &music,
                &options(SyncScope::All),
                &stop,
                &mut |_| {}
            ),
            Err(Error::Cancelled)
        );
        sync(
            &client,
            &music,
            &options(SyncScope::All),
            &never,
            &mut |_| {},
        )
        .unwrap();
        let other = fake::client(
            &root,
            Account::subsonic_token("http://other:4533".into(), "kim".into(), "x"),
        );
        let moved = sync(
            &other,
            &music,
            &options(SyncScope::All),
            &never,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!((moved.removed, moved.downloaded), (2, 2));
        assert_eq!(load_manifest(&music).username, "kim");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_lost_connection_keeps_the_list_of_what_was_copied() {
        let root = dir("interrupted");
        let music = root.join("music");
        let songs = vec![
            song("s1", "One", 10),
            song("down", "Two", 20),
            song("s3", "Three", 30),
        ];
        let client = fake::server(&root, &songs, &[]);
        let never = AtomicBool::new(false);
        let result = sync(
            &client,
            &music,
            &options(SyncScope::All),
            &never,
            &mut |_| {},
        );
        assert!(matches!(result, Err(Error::Offline(_))), "{result:?}");
        let manifest = load_manifest(&music);
        assert_eq!(manifest.songs.keys().collect::<Vec<_>>(), ["s1"]);
        assert_eq!(manifest.synced_at, 0, "not a complete sync");
        assert!(!folder(&music)
            .join("Bauhaus/Mask/.01 Two.flac.part")
            .exists());
        fs::remove_dir_all(root).unwrap();
    }
}
