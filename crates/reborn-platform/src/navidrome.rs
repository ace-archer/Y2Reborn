//! Navidrome: songs and playlists copied from a Navidrome (Subsonic API)
//! server to Internal Storage or the SD card.
//!
//! Requests go through `curl` (http or https; a server at home often has no
//! certificate). The address with its sign-in travels on curl's standard
//! input as a config line (`-K -`), never on its command line, so no token
//! shows in the process list. Sign-in is Subsonic's token scheme: the
//! password is used once to make `md5(password + salt)` and is never stored;
//! the token and salt live only in `/data/reborn/state/navidrome.json`
//! (0600). Nothing here logs a secret.
//!
//! Everything synced lives in one `Navidrome` folder on the chosen storage:
//! `Artist/Album/NN Title.ext` and `Playlists/<name>.m3u8`, with
//! `.sync.json` listing what the sync put there. Only files in that list are
//! ever replaced or removed; the server is the source, so a synced playlist
//! changed on the player is written over by the next sync.
use crate::lastfm::{form_encode, hex, md5};
use reborn_core::navidrome::{SyncQuality, SyncScope};
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
pub const FOLDER: &str = "Navidrome";
const MANIFEST: &str = ".sync.json";
const PLAYLISTS: &str = "Playlists";
/// Free space a sync leaves on the storage.
pub const RESERVE: u64 = 256 * 1024 * 1024;
/// Subsonic API version asked for (Navidrome speaks 1.16.1).
const API_VERSION: &str = "1.16.1";
const CLIENT: &str = "Y2Reborn";
/// Songs per `search3` page (Navidrome's largest).
const PAGE: usize = 500;
/// At most this many songs are listed.
const MAX_SONGS: usize = 200_000;
/// The manifest is written at most this often during a sync (and when it
/// stops), so an interruption keeps what was copied without rewriting a
/// large library's list after every few songs.
const SAVE_EVERY: Duration = Duration::from_secs(60);
/// Longest name of one folder or file (FAT32 and exFAT allow 255).
const MAX_NAME_CHARS: usize = 80;
const CURL: &str = "/usr/bin/curl";

fn curl() -> String {
    std::env::var("REBORN_CURL").unwrap_or_else(|_| CURL.into())
}

// --------------------------------------------------------------- account

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Account {
    /// `http(s)://host[:port][/path]`, no trailing slash.
    pub server: String,
    pub username: String,
    pub salt: String,
    /// `md5(password + salt)`: signs every request.
    pub token: String,
}
impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Account {{ server: {:?}, username: {:?}, .. }}",
            self.server, self.username
        )
    }
}
impl Account {
    /// A sign-in for `server` (checked with `server_address`); the password
    /// is used for the token here and nowhere else.
    pub fn new(server: String, username: String, password: &str) -> Self {
        let salt = format!("{:016x}", crate::random_u64());
        let token = hex(&md5(format!("{password}{salt}").as_bytes()));
        Self {
            server,
            username,
            salt,
            token,
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
    root.join("state/navidrome.json")
}
pub fn load_account(path: &Path) -> Option<Account> {
    let a: Account = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    (server_address(&a.server).is_some() && !a.username.is_empty() && !a.token.is_empty())
        .then_some(a)
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
    /// 40: wrong username or password.
    WrongLogin,
    /// 41, 42: the server does not take token sign-in (LDAP, API keys only).
    TokenRefused,
    /// The address answers, but not as a Subsonic server.
    NotSubsonic,
    /// Anything else the server refuses.
    Refused(i64, String),
    /// Writing on the player failed (a card removed, a read-only storage).
    Storage(String),
    Cancelled,
}
impl Error {
    pub fn retry_later(&self) -> bool {
        matches!(self, Self::Offline(_))
    }
    /// For the listener, in Reborn's words.
    pub fn message(&self) -> String {
        match self {
            Self::Offline(_) => "The Navidrome server can't be reached".into(),
            Self::WrongLogin => "Wrong Navidrome username or password".into(),
            Self::TokenRefused => "This server doesn't accept this sign-in".into(),
            Self::NotSubsonic => "That address isn't a Navidrome server".into(),
            Self::Refused(code, _) => format!("Navidrome refused the request ({code})"),
            Self::Storage(_) => "Couldn't write the music to storage".into(),
            Self::Cancelled => "Sync stopped".into(),
        }
    }
}
fn classify(code: i64, message: &str) -> Error {
    match code {
        40 => Error::WrongLogin,
        41 | 42 => Error::TokenRefused,
        _ => Error::Refused(code, message.chars().take(120).collect()),
    }
}
fn storage(e: io::Error) -> Error {
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
    fn of(v: &Value) -> Option<Self> {
        if v.get("isDir").and_then(Value::as_bool) == Some(true)
            || v.get("isVideo").and_then(Value::as_bool) == Some(true)
        {
            return None;
        }
        let text = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
        let number = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
        let id = text("id");
        if id.is_empty() {
            return None;
        }
        let album_artist = [text("displayAlbumArtist"), text("albumArtist")]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or_default();
        Some(Self {
            id,
            title: text("title"),
            album: text("album"),
            artist: text("artist"),
            album_artist,
            track: number("track") as u32,
            disc: number("discNumber") as u32,
            suffix: text("suffix").to_ascii_lowercase(),
            size: number("size"),
            duration_s: number("duration") as u32,
            bitrate_kbps: number("bitRate") as u32,
        })
    }
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
fn list(v: Option<&Value>) -> Vec<&Value> {
    match v {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(item @ Value::Object(_)) => vec![item],
        _ => vec![],
    }
}

// ---------------------------------------------------------------- client

#[derive(Debug, Clone)]
pub struct Client {
    pub account: Account,
    /// The curl program (tests use a stand-in).
    pub curl: String,
}
impl Client {
    pub fn new(account: Account) -> Self {
        Self {
            account,
            curl: curl(),
        }
    }
    fn url(&self, endpoint: &str, params: &[(&str, &str)]) -> String {
        let a = &self.account;
        let mut url = format!(
            "{}/rest/{endpoint}?u={}&t={}&s={}&v={API_VERSION}&c={CLIENT}&f=json",
            a.server,
            form_encode(&a.username),
            a.token,
            a.salt
        );
        for (k, v) in params {
            url.push_str(&format!("&{k}={}", form_encode(v)));
        }
        url
    }
    /// curl for http(s) only, redirects followed, the address on stdin.
    fn command(&self) -> Command {
        let mut command = Command::new(&self.curl);
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
            .args(["--config", "-"])
            .stdin(Stdio::piped())
            .stderr(Stdio::null());
        command
    }
    fn send_url(child: &mut Child, url: &str) -> Result<(), Error> {
        let line = format!(
            "url = \"{}\"\n",
            url.replace('\\', "\\\\").replace('"', "\\\"")
        );
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Offline("no stdin".into()))?;
        stdin
            .write_all(line.as_bytes())
            .map_err(|e| Error::Offline(e.to_string()))
    }
    fn call(&self, endpoint: &str, params: &[(&str, &str)]) -> Result<Value, Error> {
        let mut child = self
            .command()
            .args(["--max-time", "60"])
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| Error::Offline(e.to_string()))?;
        Self::send_url(&mut child, &self.url(endpoint, params))?;
        let mut reply = vec![];
        if let Some(mut out) = child.stdout.take() {
            let _ = out.by_ref().take(32 * 1024 * 1024).read_to_end(&mut reply);
        }
        let status = child.wait().map_err(|e| Error::Offline(e.to_string()))?;
        if !status.success() && reply.is_empty() {
            return Err(Error::Offline(format!(
                "curl {}",
                status.code().unwrap_or(-1)
            )));
        }
        response(&reply)
    }
    /// The sign-in works.
    pub fn ping(&self) -> Result<(), Error> {
        self.call("ping", &[]).map(|_| ())
    }
    /// Every song, a page at a time (Navidrome lists all for an empty search).
    pub fn songs(&self, cancel: &AtomicBool) -> Result<Vec<Song>, Error> {
        let mut songs = vec![];
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let offset = songs.len().to_string();
            let count = PAGE.to_string();
            let v = self.call(
                "search3",
                &[
                    ("query", ""),
                    ("artistCount", "0"),
                    ("albumCount", "0"),
                    ("songCount", &count),
                    ("songOffset", &offset),
                ],
            )?;
            let page = list(v.get("searchResult3").and_then(|r| r.get("song")));
            let n = page.len();
            songs.extend(page.into_iter().filter_map(Song::of));
            if n < PAGE || songs.len() >= MAX_SONGS {
                return Ok(songs);
            }
        }
    }
    /// Every playlist the account sees, with its songs.
    pub fn playlists(&self, cancel: &AtomicBool) -> Result<Vec<RemotePlaylist>, Error> {
        let v = self.call("getPlaylists", &[])?;
        let mut out = vec![];
        for p in list(v.get("playlists").and_then(|p| p.get("playlist"))) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let Some(id) = p.get("id").and_then(Value::as_str) else {
                continue;
            };
            let full = self.call("getPlaylist", &[("id", id)])?;
            let full = full.get("playlist");
            out.push(RemotePlaylist {
                id: id.to_owned(),
                name: p
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("Playlist")
                    .to_owned(),
                songs: list(full.and_then(|f| f.get("entry")))
                    .into_iter()
                    .filter_map(Song::of)
                    .collect(),
            });
        }
        Ok(out)
    }
    /// Download one song into `to` (written whole or not at all is the
    /// caller's rename). Stops at once when `cancel` is set.
    pub fn download(
        &self,
        song: &Song,
        quality: SyncQuality,
        to: &Path,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        let url = match song.transcode(quality) {
            Some(kbps) => self.url(
                "stream",
                &[
                    ("id", &song.id),
                    ("format", "mp3"),
                    ("maxBitRate", &kbps.to_string()),
                ],
            ),
            None => self.url("download", &[("id", &song.id)]),
        };
        let mut child = self
            .command()
            // A stalled transfer (under 1 kB/s for a minute) is given up.
            .args(["--fail", "--speed-limit", "1024", "--speed-time", "60"])
            .arg("--output")
            .arg(to)
            .stdout(Stdio::null())
            .spawn()
            .map_err(|e| Error::Offline(e.to_string()))?;
        Self::send_url(&mut child, &url)?;
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
        match status.code() {
            Some(0) => {}
            Some(22) => return Err(Error::Refused(70, "download refused".into())),
            Some(23) => return Err(Error::Storage("curl could not write".into())),
            code => return Err(Error::Offline(format!("curl {}", code.unwrap_or(-1)))),
        }
        // Subsonic answers a refused download with an error document.
        let len = fs::metadata(to).map_err(storage)?.len();
        if len < 4096 {
            let head = fs::read(to).map_err(storage)?;
            if head.starts_with(b"{") {
                response(&head)?;
            }
        }
        Ok(())
    }
}

/// The `subsonic-response` of a reply, or its error.
fn response(reply: &[u8]) -> Result<Value, Error> {
    let v: Value = serde_json::from_slice(reply).map_err(|_| Error::NotSubsonic)?;
    let mut r = v
        .get("subsonic-response")
        .cloned()
        .ok_or(Error::NotSubsonic)?;
    match r.get("status").and_then(Value::as_str) {
        Some("ok") => Ok(r.take()),
        Some(_) => {
            let e = r.get("error");
            Err(classify(
                e.and_then(|e| e.get("code"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
                e.and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            ))
        }
        None => Err(Error::NotSubsonic),
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

/// What the sync put in its folder (`Navidrome/.sync.json`). It lives with
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
fn save_manifest(root: &Path, manifest: &Manifest) -> Result<(), Error> {
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

/// Bring `root/Navidrome` in line with the server: copy what is missing or
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
    let account = &client.account;
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
                Err(Error::Refused(..) | Error::NotSubsonic) => {
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
            name = format!("{name} (Navidrome)");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("reborn-navidrome-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn song(id: &str, title: &str, size: u64) -> Song {
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
        let a = Account::new(
            "https://music.example.com/nd".into(),
            "luca".into(),
            "hunter2",
        );
        assert_eq!(a.token, hex(&md5(format!("hunter2{}", a.salt).as_bytes())));
        assert_eq!(a.host(), "music.example.com");
        assert!(!format!("{a:?}").contains(&a.token));
        save_account(&path, &a).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("hunter2"), "the password is never stored");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(load_account(&path), Some(a));
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

    /// A stand-in Navidrome behind a stand-in curl: answers by endpoint,
    /// records every config it was sent and every command line.
    fn fake_server(root: &Path, songs: &[Song], playlist: &[&str]) -> Client {
        let entry = |s: &Song| {
            serde_json::json!({"id": s.id, "title": s.title, "album": s.album,
                "artist": s.artist, "track": s.track, "suffix": s.suffix,
                "size": s.size, "duration": s.duration_s, "bitRate": s.bitrate_kbps})
        };
        let ok = |body: Value| {
            let mut r = serde_json::json!({"status": "ok", "version": "1.16.1"});
            r.as_object_mut()
                .unwrap()
                .extend(body.as_object().unwrap().clone());
            serde_json::json!({ "subsonic-response": r }).to_string()
        };
        let in_list: Vec<Value> = playlist
            .iter()
            .filter_map(|id| songs.iter().find(|s| s.id == *id))
            .map(entry)
            .collect();
        for (file, body) in [
            ("ping.json", ok(serde_json::json!({}))),
            (
                "songs.json",
                ok(
                    serde_json::json!({"searchResult3": {"song": songs.iter().map(entry).collect::<Vec<_>>()}}),
                ),
            ),
            (
                "playlists.json",
                ok(serde_json::json!({"playlists": {"playlist": [
                    {"id": "p1", "name": "Road/Trip"},
                    {"id": "p2", "name": "favorites"}
                ]}})),
            ),
            (
                "playlist.json",
                ok(serde_json::json!({"playlist": {"id": "p1", "entry": in_list}})),
            ),
        ] {
            fs::write(root.join(file), body).unwrap();
        }
        let script = root.join("curl");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$@\" >> {r}/argv\nout=''\n\
                 while [ $# -gt 0 ]; do [ \"$1\" = --output ] && out=\"$2\"; shift; done\n\
                 cfg=$(cat)\necho \"$cfg\" >> {r}/requests\n\
                 case \"$cfg\" in\n\
                 *rest/ping*) cat {r}/ping.json ;;\n\
                 *rest/getPlaylists*) cat {r}/playlists.json ;;\n\
                 *rest/getPlaylist\\?*) cat {r}/playlist.json ;;\n\
                 *rest/search3*songOffset=0*) cat {r}/songs.json ;;\n\
                 *rest/search3*) echo '{{\"subsonic-response\":{{\"status\":\"ok\"}}}}' ;;\n\
                 *id=down*) exit 7 ;;\n\
                 *id=bad*) echo '{{\"subsonic-response\":{{\"status\":\"failed\",\"error\":{{\"code\":70,\"message\":\"not found\"}}}}}}' > \"$out\" ;;\n\
                 *rest/download*|*rest/stream*) printf 'audio %s' \"$cfg\" > \"$out\" ;;\n\
                 esac\n",
                r = root.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        Client {
            curl: script.display().to_string(),
            ..Client::new(Account::new(
                "http://nd.local:4533".into(),
                "luca".into(),
                "pass word",
            ))
        }
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
        let client = fake_server(&root, &songs, &["s2", "s1"]);
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
        assert!(sync_dir
            .join("Playlists/favorites (Navidrome).m3u8")
            .is_file());
        assert!(!sync_dir.join("Playlists/favorites.m3u8").exists());
        // No secret on any command line; the token goes on stdin.
        let argv = fs::read_to_string(root.join("argv")).unwrap();
        assert!(!argv.contains(&client.account.token) && argv.contains("--config -"));
        assert!(fs::read_to_string(root.join("requests"))
            .unwrap()
            .contains(&format!("t={}", client.account.token)));
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
        let client = fake_server(&root, &changed, &["s2"]);
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
        let client = fake_server(&root, &songs, &[]);
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
        let other = Client {
            account: Account::new("http://other:4533".into(), "kim".into(), "x"),
            ..client.clone()
        };
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
        let client = fake_server(&root, &songs, &[]);
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

    #[test]
    fn errors_classify() {
        let failed = |code: i64| {
            format!(
                r#"{{"subsonic-response":{{"status":"failed","error":{{"code":{code},"message":"m"}}}}}}"#
            )
        };
        assert_eq!(response(failed(40).as_bytes()), Err(Error::WrongLogin));
        assert_eq!(response(failed(41).as_bytes()), Err(Error::TokenRefused));
        assert_eq!(
            response(failed(70).as_bytes()),
            Err(Error::Refused(70, "m".into()))
        );
        assert_eq!(response(b"<html>"), Err(Error::NotSubsonic));
        assert_eq!(response(b"{}"), Err(Error::NotSubsonic));
        assert!(Error::Offline("x".into()).retry_later() && !Error::WrongLogin.retry_later());
    }
}
