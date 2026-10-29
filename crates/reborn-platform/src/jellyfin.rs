//! Jellyfin's REST API for `music_server`. Sign-in trades the username and
//! password for an access token for this player: the password goes once,
//! in the body of `/Users/AuthenticateByName`, and is never kept. Every
//! request after that carries the token in the `Authorization` header (the
//! legacy `X-Emby-Token` and `api_key` ways are off since Jellyfin 12).
use crate::lastfm::form_encode;
use crate::music_server::{list, Account, Auth, Curl, Error, Playlist, Request, Song, MAX_SONGS};
use reborn_core::music_server::SyncQuality;
use serde_json::Value;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

const CLIENT: &str = "Y2Reborn";
const DEVICE: &str = "Innioasis Y2";
/// Songs per page: each comes with its media sources, so pages stay small.
const PAGE: usize = 200;

/// The server answers its public info as Jellyfin. Only no connection at all
/// is an error: anything else is some other server.
pub(crate) fn is_jellyfin(curl: &Curl, server: &str) -> Result<bool, Error> {
    let (status, reply) = curl.fetch(&Request::get(format!("{server}/System/Info/Public")))?;
    Ok(status == 200
        && serde_json::from_slice::<Value>(&reply)
            .ok()
            .and_then(|v| {
                v.get("ProductName")?
                    .as_str()
                    .map(|p| p.contains("Jellyfin"))
            })
            .unwrap_or(false))
}

/// `Authorization: MediaBrowser …`, naming this player, with its token once
/// signed in.
fn authorization(device_id: &str, token: Option<&str>) -> String {
    let mut header = format!(
        "Authorization: MediaBrowser Client=\"{CLIENT}\", Device=\"{DEVICE}\", \
         DeviceId=\"{device_id}\", Version=\"{}\"",
        env!("CARGO_PKG_VERSION")
    );
    if let Some(token) = token {
        header.push_str(&format!(", Token=\"{token}\""));
    }
    header
}

pub(crate) fn sign_in(
    curl: &Curl,
    server: String,
    username: String,
    password: &str,
) -> Result<Account, Error> {
    let device_id = format!("{:016x}", crate::random_u64());
    let (status, reply) = curl.fetch(&Request {
        url: format!("{server}/Users/AuthenticateByName"),
        headers: vec![
            authorization(&device_id, None),
            "Content-Type: application/json".into(),
        ],
        body: Some(serde_json::json!({"Username": username, "Pw": password}).to_string()),
    })?;
    let v = answer(status, &reply)?;
    let text = |v: Option<&Value>| v.and_then(Value::as_str).unwrap_or("").to_owned();
    let token = text(v.get("AccessToken"));
    let user_id = text(v.get("User").and_then(|u| u.get("Id")));
    if token.is_empty() || user_id.is_empty() {
        return Err(Error::NotAServer);
    }
    downloads_allowed(v.get("User"))?;
    Ok(Account {
        server,
        username,
        auth: Auth::Jellyfin {
            user_id,
            device_id,
            token,
        },
    })
}

/// A reply's JSON, or the error its status means.
fn answer(status: u16, reply: &[u8]) -> Result<Value, Error> {
    match status {
        200..=299 => serde_json::from_slice(reply).map_err(|_| Error::NotAServer),
        401 => Err(Error::WrongLogin),
        code => Err(Error::Refused(
            code.into(),
            String::from_utf8_lossy(reply).chars().take(120).collect(),
        )),
    }
}

/// An administrator can keep an account from downloading, and a sync is
/// downloading.
fn downloads_allowed(user: Option<&Value>) -> Result<(), Error> {
    let allowed = user
        .and_then(|u| u.get("Policy"))
        .and_then(|p| p.get("EnableContentDownloading"))
        .and_then(Value::as_bool);
    if allowed == Some(false) {
        Err(Error::NoDownloads)
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Client {
    pub(crate) account: Account,
    pub(crate) curl: Curl,
}
impl Client {
    fn request(&self, path: &str) -> Request {
        let (device_id, token) = match &self.account.auth {
            Auth::Jellyfin {
                device_id, token, ..
            } => (device_id.as_str(), Some(token.as_str())),
            _ => ("", None),
        };
        Request {
            url: format!("{}{path}", self.account.server),
            headers: vec![authorization(device_id, token)],
            body: None,
        }
    }
    fn get(&self, path: &str) -> Result<Value, Error> {
        let (status, reply) = self.curl.fetch(&self.request(path))?;
        answer(status, &reply)
    }
    fn user_id(&self) -> String {
        match &self.account.auth {
            Auth::Jellyfin { user_id, .. } => form_encode(user_id),
            _ => String::new(),
        }
    }
    pub fn ping(&self) -> Result<(), Error> {
        let me = self.get("/Users/Me")?;
        downloads_allowed(Some(&me))
    }
    pub fn songs(&self, cancel: &AtomicBool) -> Result<Vec<Song>, Error> {
        self.audio("", cancel)
    }
    /// The songs marked favorite by this account.
    pub fn favorites(&self, cancel: &AtomicBool) -> Result<Vec<Song>, Error> {
        self.audio("&Filters=IsFavorite", cancel)
    }
    /// Every song, `filter` added to the query, in pages.
    fn audio(&self, filter: &str, cancel: &AtomicBool) -> Result<Vec<Song>, Error> {
        let user = self.user_id();
        let mut songs = vec![];
        let mut start = 0;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let v = self.get(&format!(
                "/Items?userId={user}&IncludeItemTypes=Audio&Recursive=true{filter}\
                 &Fields=MediaSources&SortBy=SortName&EnableImages=false\
                 &EnableUserData=false&StartIndex={start}&Limit={PAGE}"
            ))?;
            let page = list(v.get("Items"));
            start += page.len();
            let n = page.len();
            songs.extend(page.into_iter().filter_map(song));
            if n < PAGE || songs.len() >= MAX_SONGS {
                return Ok(songs);
            }
        }
    }
    pub fn playlist_list(&self) -> Result<Vec<Playlist>, Error> {
        let user = self.user_id();
        let v = self.get(&format!(
            "/Items?userId={user}&IncludeItemTypes=Playlist&Recursive=true&Fields=ChildCount"
        ))?;
        Ok(list(v.get("Items"))
            .into_iter()
            // Video playlists hold nothing for the player.
            .filter(|p| p.get("MediaType").and_then(Value::as_str) != Some("Video"))
            .filter_map(|p| {
                Some(Playlist {
                    id: p.get("Id")?.as_str()?.to_owned(),
                    name: p
                        .get("Name")
                        .and_then(Value::as_str)
                        .unwrap_or("Playlist")
                        .to_owned(),
                    songs: p
                        .get("ChildCount")
                        .and_then(Value::as_u64)
                        .map(|n| n as usize),
                })
            })
            .collect())
    }
    pub fn playlist_songs(&self, id: &str) -> Result<Vec<Song>, Error> {
        let user = self.user_id();
        let entries = self.get(&format!(
            "/Playlists/{}/Items?userId={user}&Fields=MediaSources",
            form_encode(id)
        ))?;
        Ok(list(entries.get("Items"))
            .into_iter()
            .filter_map(song)
            .collect())
    }
    pub fn download(
        &self,
        song: &Song,
        quality: SyncQuality,
        to: &Path,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        let id = form_encode(&song.id);
        let path = match song.transcode(quality) {
            Some(kbps) => format!(
                "/Audio/{id}/stream?container=mp3&audioCodec=mp3&audioBitRate={}\
                 &audioChannels=2&static=false",
                kbps * 1000
            ),
            None => format!("/Items/{id}/Download"),
        };
        match self.curl.download(&self.request(&path), to, cancel)? {
            200..=299 => Ok(()),
            401 => Err(Error::WrongLogin),
            code => Err(Error::Refused(code.into(), "download refused".into())),
        }
    }
}

/// One song as Jellyfin lists it.
fn song(v: &Value) -> Option<Song> {
    if v.get("Type")
        .and_then(Value::as_str)
        .is_some_and(|t| t != "Audio")
    {
        return None;
    }
    let text = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
    let number = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    let id = text("Id");
    if id.is_empty() {
        return None;
    }
    let source = v.get("MediaSources").and_then(|m| m.get(0));
    let of_source = |k: &str| source.and_then(|s| s.get(k)).and_then(Value::as_u64);
    let artists: Vec<&str> = list(v.get("Artists"))
        .into_iter()
        .filter_map(Value::as_str)
        .collect();
    Some(Song {
        id,
        title: text("Name"),
        album: text("Album"),
        artist: if artists.is_empty() {
            text("AlbumArtist")
        } else {
            artists.join(", ")
        },
        album_artist: text("AlbumArtist"),
        track: number("IndexNumber") as u32,
        disc: number("ParentIndexNumber") as u32,
        suffix: suffix(source, v),
        size: of_source("Size").unwrap_or(0),
        // Ticks of 100 ns.
        duration_s: (number("RunTimeTicks") / 10_000_000) as u32,
        bitrate_kbps: of_source("Bitrate").map_or(0, |bps| (bps / 1000) as u32),
    })
}

/// The file's extension: from its path when the server shows it, else from
/// its container ("flac"; "mov,mp4,m4a,…" is an .m4a).
fn suffix(source: Option<&Value>, item: &Value) -> String {
    let from_path = source
        .and_then(|s| s.get("Path"))
        .and_then(Value::as_str)
        .and_then(|p| p.rsplit_once('.'))
        .map(|(_, ext)| ext)
        .filter(|ext| {
            (1..=5).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric())
        });
    if let Some(ext) = from_path {
        return ext.to_ascii_lowercase();
    }
    let container = source
        .and_then(|s| s.get("Container"))
        .or(item.get("Container"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    if container.split(',').any(|c| c == "m4a") {
        "m4a".into()
    } else {
        container.split(',').next().unwrap_or("").to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::music_server::testing::{self, dir};
    use crate::music_server::{
        load_manifest, sign_in_with, sync, Client as AnyClient, Options, FOLDER,
    };
    use reborn_core::music_server::{SyncChoice, SyncScope};
    use std::fs;

    fn item(id: &str, name: &str, track: u32, path: &str) -> Value {
        serde_json::json!({"Id": id, "Name": name, "Type": "Audio",
            "Album": "Mask", "AlbumArtist": "Bauhaus", "Artists": ["Bauhaus"],
            "IndexNumber": track, "RunTimeTicks": 2_000_000_000u64,
            "MediaSources": [{"Path": path, "Container": "flac", "Size": 1000, "Bitrate": 900_000}]})
    }

    /// A stand-in Jellyfin: public info, sign-in (password "right"), one
    /// user that may download unless `no_downloads`, two songs, one audio
    /// playlist and a video one.
    fn server(root: &Path, no_downloads: bool) -> Curl {
        let songs = serde_json::json!({"Items": [
            item("a1", "Hollow Hills", 1, "/music/Bauhaus/Mask/01.flac"),
            item("a2", "Kick in the Eye", 2, "/music/Bauhaus/Mask/02.m4a"),
        ], "TotalRecordCount": 2});
        let user = serde_json::json!({"Id": "u1", "Name": "luca",
            "Policy": {"EnableContentDownloading": !no_downloads}});
        let auth = serde_json::json!({"AccessToken": "tok3n", "User": user});
        let playlists = serde_json::json!({"Items": [
            {"Id": "p1", "Name": "Road Trip", "MediaType": "Audio"},
            {"Id": "p2", "Name": "Clips", "MediaType": "Video"},
        ]});
        let entries = serde_json::json!({"Items": [item("a2", "Kick in the Eye", 2, "")]});
        let favorites = serde_json::json!({"Items": [item("a1", "Hollow Hills", 1, "/m/01.flac")]});
        for (file, body) in [
            ("favorites.json", favorites),
            ("songs.json", songs),
            ("user.json", user),
            ("auth.json", auth),
            ("playlists.json", playlists),
            ("entries.json", entries),
            (
                "info.json",
                serde_json::json!({"ProductName": "Jellyfin Server"}),
            ),
        ] {
            fs::write(root.join(file), body.to_string()).unwrap();
        }
        testing::curl(
            root,
            &format!(
                "*System/Info/Public*) cat {r}/info.json ;;\n\
                 *AuthenticateByName*Pw*right*) cat {r}/auth.json ;;\n\
                 *AuthenticateByName*) code=401 ;;\n\
                 *Token=*tok3n*) case \"$cfg\" in\n\
                   *Users/Me*) cat {r}/user.json ;;\n\
                   *IsFavorite*StartIndex=0*) cat {r}/favorites.json ;;\n\
                   *IncludeItemTypes=Audio*StartIndex=0*) cat {r}/songs.json ;;\n\
                   *IncludeItemTypes=Audio*) echo '{{\"Items\":[]}}' ;;\n\
                   *IncludeItemTypes=Playlist*) cat {r}/playlists.json ;;\n\
                   *Playlists/p1/Items*) cat {r}/entries.json ;;\n\
                   *Download*|*Audio/*/stream*) printf 'audio' > \"$out\" ;;\n\
                   *) code=404 ;;\n\
                   esac ;;\n\
                 *) code=401 ;;",
                r = root.display()
            ),
        )
    }

    #[test]
    fn jellyfin_signs_in_with_a_token_and_syncs() {
        let root = dir("jellyfin");
        let curl = server(&root, false);
        assert_eq!(
            sign_in_with(&curl, "http://jf.local:8096".into(), "luca".into(), "wrong"),
            Err(Error::WrongLogin)
        );
        let account =
            sign_in_with(&curl, "http://jf.local:8096".into(), "luca".into(), "right").unwrap();
        let Auth::Jellyfin { user_id, token, .. } = &account.auth else {
            panic!("{account:?}")
        };
        assert_eq!((user_id.as_str(), token.as_str()), ("u1", "tok3n"));
        // Nothing secret on a command line; the password went once, on stdin.
        let argv = fs::read_to_string(root.join("argv")).unwrap();
        assert!(!argv.contains("right") && !argv.contains("tok3n"));
        let asked = fs::read_to_string(root.join("requests")).unwrap();
        assert!(asked.contains("header = \"Authorization: MediaBrowser Client=\\\"Y2Reborn\\\""));
        assert!(asked.contains(r#"data-raw = "{\"Pw\":\"right\",\"Username\":\"luca\"}""#));

        let music = root.join("music");
        let client = AnyClient::with(account, curl);
        let never = AtomicBool::new(false);
        let options = Options {
            scope: SyncScope::All,
            choice: SyncChoice::default(),
            quality: SyncQuality::Original,
            keep: false,
            now_unix: 1_800_000_000,
            reserve: 0,
        };
        let summary = sync(&client, &music, &options, &never, &mut |_| {}).unwrap();
        assert_eq!((summary.downloaded, summary.playlists), (2, 1));
        let synced = music.join(FOLDER);
        assert!(synced.join("Bauhaus/Mask/01 Hollow Hills.flac").is_file());
        assert!(synced.join("Bauhaus/Mask/02 Kick in the Eye.m4a").is_file());
        assert_eq!(
            fs::read_to_string(synced.join("Playlists/Road Trip.m3u8")).unwrap(),
            "#EXTM3U\n#EXTINF:200,Bauhaus - Kick in the Eye\n../Bauhaus/Mask/02 Kick in the Eye.m4a\n"
        );
        assert_eq!(load_manifest(&music).username, "luca");
        let asked = fs::read_to_string(root.join("requests")).unwrap();
        assert!(asked.contains("/Items/a1/Download"));

        // MP3: the server transcodes.
        fs::remove_dir_all(&music).unwrap();
        let mp3 = Options {
            quality: SyncQuality::Mp3Medium,
            ..options.clone()
        };
        sync(&client, &music, &mp3, &never, &mut |_| {}).unwrap();
        let asked = fs::read_to_string(root.join("requests")).unwrap();
        assert!(asked.contains("/Audio/a1/stream?container=mp3&audioCodec=mp3&audioBitRate=192000"));

        // Favorites only: the favorite song, no playlist asked for.
        fs::remove_dir_all(&music).unwrap();
        fs::remove_file(root.join("requests")).unwrap();
        let favorites = Options {
            scope: SyncScope::Selected,
            choice: SyncChoice {
                favorites: true,
                all_playlists: false,
                ..SyncChoice::default()
            },
            ..options
        };
        let summary = sync(&client, &music, &favorites, &never, &mut |_| {}).unwrap();
        assert_eq!(
            (summary.downloaded, summary.songs, summary.playlists),
            (1, 1, 0)
        );
        assert!(synced.join("Bauhaus/Mask/01 Hollow Hills.flac").is_file());
        let asked = fs::read_to_string(root.join("requests")).unwrap();
        assert!(asked.contains("Filters=IsFavorite") && !asked.contains("Playlists/p1/Items"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_account_that_may_not_download_is_told_so() {
        let root = dir("jellyfin-no-downloads");
        let curl = server(&root, true);
        assert_eq!(
            sign_in_with(&curl, "http://jf.local".into(), "luca".into(), "right"),
            Err(Error::NoDownloads)
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn songs_read_their_file_type_from_path_or_container() {
        let mut v = item("x", "T", 1, "/m/a.b/Song.OPUS");
        assert_eq!(song(&v).unwrap().suffix, "opus");
        v["MediaSources"][0]["Path"] = "/m/a.b/no-extension".into();
        v["MediaSources"][0]["Container"] = "mov,mp4,m4a,3gp,3g2,mj2".into();
        assert_eq!(song(&v).unwrap().suffix, "m4a");
        let s = song(&v).unwrap();
        assert_eq!((s.duration_s, s.bitrate_kbps, s.size), (200, 900, 1000));
        v["Type"] = "MusicVideo".into();
        assert_eq!(song(&v), None);
    }
}
