//! The Subsonic API (Navidrome, Gonic, Airsonic, Ampache, LMS and others) for
//! `music_server`. Requests are signed with Subsonic's token
//! (`md5(password + salt)`), or with an OpenSubsonic API key for a server
//! that refuses tokens (one that keeps only a hash of the password, or signs
//! in through LDAP); the key is typed where the password goes.
use crate::lastfm::form_encode;
use crate::music_server::{
    list, storage, Account, Auth, Curl, Error, RemotePlaylist, Request, Song, MAX_SONGS,
};
use reborn_core::music_server::SyncQuality;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// API version asked for: the first with token sign-in, and new enough for
/// everything used here, so older servers answer it too.
const API_VERSION: &str = "1.13.0";
const CLIENT: &str = "Y2Reborn";
/// Songs per `search3` page, albums per `getAlbumList2` page (the largest
/// the API allows).
const PAGE: usize = 500;

/// Sign in with a token; a server that refuses tokens but offers API keys
/// (OpenSubsonic's `apiKeyAuthentication`) is asked with the password as
/// its key.
pub(crate) fn sign_in(
    curl: &Curl,
    server: String,
    username: String,
    password: &str,
) -> Result<Account, Error> {
    let client = Client {
        account: Account::subsonic_token(server.clone(), username.clone(), password),
        curl: curl.clone(),
    };
    match client.ping() {
        Err(Error::TokenRefused) if takes_api_keys(curl, &server) => {
            let keyed = Client {
                account: Account {
                    server,
                    username,
                    auth: Auth::SubsonicKey {
                        key: password.to_owned(),
                    },
                },
                curl: curl.clone(),
            };
            keyed.ping().map(|_| keyed.account)
        }
        checked => checked.map(|_| client.account),
    }
}

/// The server lists `apiKeyAuthentication` among its OpenSubsonic
/// extensions (a list it gives without a sign-in).
fn takes_api_keys(curl: &Curl, server: &str) -> bool {
    let reply = curl.fetch(&Request::get(address(
        server,
        "getOpenSubsonicExtensions",
        "",
        &[],
    )));
    reply
        .ok()
        .and_then(|(_, body)| response(&body).ok())
        .is_some_and(|v| {
            list(v.get("openSubsonicExtensions"))
                .iter()
                .any(|e| e.get("name").and_then(Value::as_str) == Some("apiKeyAuthentication"))
        })
}

/// `server/rest/endpoint?…` with the sign-in (`auth`, already encoded) and
/// `params`.
fn address(server: &str, endpoint: &str, auth: &str, params: &[(&str, &str)]) -> String {
    let mut url = format!("{server}/rest/{endpoint}?v={API_VERSION}&c={CLIENT}&f=json");
    if !auth.is_empty() {
        url.push('&');
        url.push_str(auth);
    }
    for (k, v) in params {
        url.push_str(&format!("&{k}={}", form_encode(v)));
    }
    url
}

#[derive(Debug, Clone)]
pub struct Client {
    pub(crate) account: Account,
    pub(crate) curl: Curl,
}
impl Client {
    fn url(&self, endpoint: &str, params: &[(&str, &str)]) -> String {
        let a = &self.account;
        let auth = match &a.auth {
            Auth::SubsonicToken { salt, token } => {
                format!("u={}&t={token}&s={salt}", form_encode(&a.username))
            }
            // With a key, no username: OpenSubsonic refuses both at once.
            Auth::SubsonicKey { key } => format!("apiKey={}", form_encode(key)),
            Auth::Jellyfin { .. } => String::new(),
        };
        address(&a.server, endpoint, &auth, params)
    }
    fn call(&self, endpoint: &str, params: &[(&str, &str)]) -> Result<Value, Error> {
        let (_, reply) = self.curl.fetch(&Request::get(self.url(endpoint, params)))?;
        response(&reply)
    }
    pub fn ping(&self) -> Result<(), Error> {
        self.call("ping", &[]).map(|_| ())
    }
    /// Every song: an empty search lists them all on most servers
    /// (Navidrome, Gonic and others), a page at a time; a server that
    /// refuses it or finds nothing is walked album by album instead.
    pub fn songs(&self, cancel: &AtomicBool) -> Result<Vec<Song>, Error> {
        match self.searched_songs(cancel) {
            Ok(songs) if !songs.is_empty() => Ok(songs),
            Ok(_) | Err(Error::Refused(..)) => self.album_songs(cancel),
            Err(e) => Err(e),
        }
    }
    fn searched_songs(&self, cancel: &AtomicBool) -> Result<Vec<Song>, Error> {
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
            songs.extend(page.into_iter().filter_map(song));
            if n < PAGE || songs.len() >= MAX_SONGS {
                return Ok(songs);
            }
        }
    }
    fn album_songs(&self, cancel: &AtomicBool) -> Result<Vec<Song>, Error> {
        let mut songs = vec![];
        let mut offset = 0;
        loop {
            let size = PAGE.to_string();
            let from = offset.to_string();
            let v = self.call(
                "getAlbumList2",
                &[
                    ("type", "alphabeticalByName"),
                    ("size", &size),
                    ("offset", &from),
                ],
            )?;
            let albums = list(v.get("albumList2").and_then(|l| l.get("album")));
            offset += albums.len();
            for album in &albums {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                let Some(id) = album.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let full = self.call("getAlbum", &[("id", id)])?;
                let found = list(full.get("album").and_then(|a| a.get("song")));
                songs.extend(found.into_iter().filter_map(song));
                if songs.len() >= MAX_SONGS {
                    return Ok(songs);
                }
            }
            if albums.len() < PAGE {
                return Ok(songs);
            }
        }
    }
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
                    .filter_map(song)
                    .collect(),
            });
        }
        Ok(out)
    }
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
        let status = self.curl.download(&Request::get(url), to, cancel)?;
        if !(200..300).contains(&status) {
            return Err(Error::Refused(status.into(), "download refused".into()));
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

/// One song as a Subsonic server lists it (folders and videos are not).
fn song(v: &Value) -> Option<Song> {
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
    Some(Song {
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

fn classify(code: i64, message: &str) -> Error {
    match code {
        // 44: an API key the server does not know.
        40 | 44 => Error::WrongLogin,
        // 41: no token sign-in (LDAP); 42: not this way of signing in.
        41 | 42 => Error::TokenRefused,
        _ => Error::Refused(code, message.chars().take(120).collect()),
    }
}

/// The `subsonic-response` of a reply, or its error.
fn response(reply: &[u8]) -> Result<Value, Error> {
    let v: Value = serde_json::from_slice(reply).map_err(|_| Error::NotAServer)?;
    let mut r = v
        .get("subsonic-response")
        .cloned()
        .ok_or(Error::NotAServer)?;
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
        None => Err(Error::NotAServer),
    }
}

/// A stand-in Subsonic server for the tests here and in `music_server`.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use crate::music_server::{testing, Client};

    /// The stand-in curl serving `songs` (and `playlist`, songs by id, as
    /// playlist p1 next to an empty "favorites"), signed in as luca.
    pub fn server(root: &Path, songs: &[Song], playlist: &[&str]) -> Client {
        let entry = |s: &Song| {
            serde_json::json!({"id": s.id, "title": s.title, "album": s.album,
                "artist": s.artist, "track": s.track, "suffix": s.suffix,
                "size": s.size, "duration": s.duration_s, "bitRate": s.bitrate_kbps})
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
                "albums.json",
                ok(serde_json::json!({"albumList2": {"album": [{"id": "al1"}]}})),
            ),
            (
                "album.json",
                ok(
                    serde_json::json!({"album": {"id": "al1", "song": songs.iter().map(entry).collect::<Vec<_>>()}}),
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
        client(
            root,
            Account::subsonic_token("http://nd.local:4533".into(), "luca".into(), "pass word"),
        )
    }

    /// The stand-in server for another account.
    pub fn client(root: &Path, account: Account) -> Client {
        Client::with(account, testing::curl(root, &arms(root)))
    }

    pub fn ok(body: Value) -> String {
        let mut r = serde_json::json!({"status": "ok", "version": "1.16.1"});
        r.as_object_mut()
            .unwrap()
            .extend(body.as_object().unwrap().clone());
        serde_json::json!({ "subsonic-response": r }).to_string()
    }

    fn arms(root: &Path) -> String {
        format!(
            "*rest/ping*) cat {r}/ping.json ;;\n\
             *rest/getPlaylists*) cat {r}/playlists.json ;;\n\
             *rest/getPlaylist\\?*) cat {r}/playlist.json ;;\n\
             *rest/search3*songOffset=0*) cat {r}/songs.json ;;\n\
             *rest/search3*) echo '{{\"subsonic-response\":{{\"status\":\"ok\"}}}}' ;;\n\
             *rest/getAlbumList2*offset=0*) cat {r}/albums.json ;;\n\
             *rest/getAlbumList2*) echo '{{\"subsonic-response\":{{\"status\":\"ok\"}}}}' ;;\n\
             *rest/getAlbum\\?*) cat {r}/album.json ;;\n\
             *id=down*) exit 7 ;;\n\
             *id=bad*) echo '{{\"subsonic-response\":{{\"status\":\"failed\",\"error\":{{\"code\":70,\"message\":\"not found\"}}}}}}' > \"$out\" ;;\n\
             *rest/download*|*rest/stream*) printf 'audio %s' \"$cfg\" > \"$out\" ;;",
            r = root.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::music_server::testing::{self, dir};

    #[test]
    fn a_server_without_an_empty_search_is_listed_album_by_album() {
        let root = dir("albums");
        let songs = [
            testing::song("aaaa", "One", 1),
            testing::song("bbbb", "Two", 2),
        ];
        let client = fake::server(&root, &songs, &[]);
        let never = AtomicBool::new(false);
        let ids = |listed: Vec<Song>| listed.into_iter().map(|s| s.id).collect::<Vec<_>>();
        assert_eq!(ids(client.songs(&never).unwrap()), ["aaaa", "bbbb"]);
        // Refused (Airsonic wants a query) or nothing found: the albums.
        for answer in [
            r#"{"subsonic-response":{"status":"failed","error":{"code":10,"message":"query"}}}"#,
            r#"{"subsonic-response":{"status":"ok","searchResult3":{}}}"#,
        ] {
            fs::write(root.join("songs.json"), answer).unwrap();
            assert_eq!(ids(client.songs(&never).unwrap()), ["aaaa", "bbbb"]);
        }
        let asked = fs::read_to_string(root.join("requests")).unwrap();
        assert!(asked.contains("getAlbumList2?") && asked.contains("type=alphabeticalByName"));
        assert!(asked.contains("getAlbum?") && asked.contains("id=al1"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_server_refusing_tokens_takes_the_password_as_an_api_key() {
        let root = dir("api-key");
        let refused =
            r#"{"subsonic-response":{"status":"failed","error":{"code":42,"message":"no"}}}"#;
        let unknown =
            r#"{"subsonic-response":{"status":"failed","error":{"code":44,"message":"no"}}}"#;
        let extensions = fake::ok(
            serde_json::json!({"openSubsonicExtensions": [{"name": "apiKeyAuthentication", "versions": [1]}]}),
        );
        let curl = testing::curl(
            &root,
            &format!(
                "*getOpenSubsonicExtensions*) echo '{extensions}' ;;\n\
                 *apiKey=k3y*) echo '{}' ;;\n\
                 *apiKey=*) echo '{unknown}' ;;\n\
                 *) echo '{refused}' ;;",
                fake::ok(serde_json::json!({}))
            ),
        );
        let account = sign_in(&curl, "http://lms.local".into(), "luca".into(), "k3y").unwrap();
        assert_eq!(account.auth, Auth::SubsonicKey { key: "k3y".into() });
        // The key is sent without the username (OpenSubsonic refuses both).
        let asked = fs::read_to_string(root.join("requests")).unwrap();
        let keyed = asked.lines().find(|l| l.contains("apiKey=")).unwrap();
        assert!(!keyed.contains("u=luca"));
        assert_eq!(
            sign_in(&curl, "http://lms.local".into(), "luca".into(), "nope"),
            Err(Error::WrongLogin)
        );
        // No API keys offered: the refusal stands.
        let curl = testing::curl(&root, &format!("*) echo '{refused}' ;;"));
        assert_eq!(
            sign_in(&curl, "http://lms.local".into(), "luca".into(), "k3y"),
            Err(Error::TokenRefused)
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn errors_classify() {
        let failed = |code: i64| {
            format!(
                r#"{{"subsonic-response":{{"status":"failed","error":{{"code":{code},"message":"m"}}}}}}"#
            )
        };
        assert_eq!(response(failed(40).as_bytes()), Err(Error::WrongLogin));
        assert_eq!(response(failed(44).as_bytes()), Err(Error::WrongLogin));
        assert_eq!(response(failed(41).as_bytes()), Err(Error::TokenRefused));
        assert_eq!(
            response(failed(70).as_bytes()),
            Err(Error::Refused(70, "m".into()))
        );
        assert_eq!(response(b"<html>"), Err(Error::NotAServer));
        assert_eq!(response(b"{}"), Err(Error::NotAServer));
        assert!(Error::Offline("x".into()).retry_later() && !Error::WrongLogin.retry_later());
    }
}
