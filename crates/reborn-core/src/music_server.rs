//! Music server sync (Settings > Storage > Music Server): the choices it keeps and
//! the requests its page sends. The server is the source: songs and
//! playlists are copied to the player, never the other way.
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Where synced music is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncTarget {
    #[default]
    Internal,
    SdCard,
}
impl SyncTarget {
    pub fn label(self) -> &'static str {
        match self {
            Self::Internal => "Internal Storage",
            Self::SdCard => "SD Card",
        }
    }
    pub fn next(self) -> Self {
        match self {
            Self::Internal => Self::SdCard,
            Self::SdCard => Self::Internal,
        }
    }
}

/// What is synced: the whole library with every playlist, or what the
/// `SyncChoice` picks (a library larger than the player).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncScope {
    #[default]
    All,
    /// Was "Playlists Only": every playlist is the choice it starts with.
    #[serde(rename = "playlists")]
    Selected,
}
impl SyncScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All Music",
            Self::Selected => "Selected Music",
        }
    }
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Selected,
            Self::Selected => Self::All,
        }
    }
}

/// What Selected Music copies: the songs marked favorite on the server and
/// the chosen playlists with their songs. Playlist ids are the server's own,
/// so the choice belongs to one account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncChoice {
    /// `username@server` the choice was made for; another account starts over.
    #[serde(default)]
    pub account: String,
    #[serde(default)]
    pub favorites: bool,
    /// Every playlist, those made later too; else only `playlists`.
    #[serde(default = "every")]
    pub all_playlists: bool,
    #[serde(default)]
    pub playlists: BTreeSet<String>,
}
fn every() -> bool {
    true
}
impl Default for SyncChoice {
    fn default() -> Self {
        Self {
            account: String::new(),
            favorites: false,
            all_playlists: true,
            playlists: BTreeSet::new(),
        }
    }
}
impl SyncChoice {
    pub fn account_key(server: &str, username: &str) -> String {
        format!("{username}@{server}")
    }
    /// The choice for `key`: this one if it was made for it, else a new one.
    pub fn for_account(&self, key: &str) -> Self {
        if self.account == key {
            self.clone()
        } else {
            Self {
                account: key.to_owned(),
                ..Self::default()
            }
        }
    }
    pub fn includes(&self, playlist: &str) -> bool {
        self.all_playlists || self.playlists.contains(playlist)
    }
    /// Nothing would be copied.
    pub fn is_empty(&self) -> bool {
        !self.favorites && !self.all_playlists && self.playlists.is_empty()
    }
    /// All Playlists on, or off with none chosen yet.
    pub fn toggle_all(&mut self) {
        self.all_playlists = !self.all_playlists;
        self.playlists.clear();
    }
    /// One playlist in or out; under All Playlists, out means every other
    /// one of `listed` stays chosen.
    pub fn toggle(&mut self, playlist: &str, listed: &[&str]) {
        if self.all_playlists {
            self.all_playlists = false;
            self.playlists = listed
                .iter()
                .filter(|id| **id != playlist)
                .map(|id| (*id).to_owned())
                .collect();
        } else if !self.playlists.remove(playlist) {
            self.playlists.insert(playlist.to_owned());
        }
    }
}

/// The file each song is kept as: the server's original, or MP3 made by the
/// server's transcoder (it must have transcoding set up).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncQuality {
    #[default]
    Original,
    Mp3High,
    Mp3Medium,
}
impl SyncQuality {
    pub fn label(self) -> &'static str {
        match self {
            Self::Original => "Original",
            Self::Mp3High => "MP3 320 kbit/s",
            Self::Mp3Medium => "MP3 192 kbit/s",
        }
    }
    pub fn next(self) -> Self {
        match self {
            Self::Original => Self::Mp3High,
            Self::Mp3High => Self::Mp3Medium,
            Self::Mp3Medium => Self::Original,
        }
    }
    /// The MP3 bit rate in kbit/s; `None` keeps the original file.
    pub fn mp3_kbps(self) -> Option<u32> {
        match self {
            Self::Original => None,
            Self::Mp3High => Some(320),
            Self::Mp3Medium => Some(192),
        }
    }
}

/// Settings > Storage > Music Server requests. A password lives only in
/// `Login` on its way to the one sign-in check and is never printed.
#[derive(Clone, PartialEq, Eq)]
pub enum MusicServerRequest {
    Login {
        server: String,
        username: String,
        password: String,
    },
    SyncNow,
    Cancel,
    Disconnect,
    /// The server's playlists, for choosing which to sync.
    ListPlaylists,
}
impl std::fmt::Debug for MusicServerRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Login { server, .. } => write!(f, "Login {{ server: {server:?}, .. }}"),
            Self::SyncNow => f.write_str("SyncNow"),
            Self::Cancel => f.write_str("Cancel"),
            Self::Disconnect => f.write_str("Disconnect"),
            Self::ListPlaylists => f.write_str("ListPlaylists"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_step_round_and_keep_secrets_out_of_debug() {
        assert_eq!(SyncTarget::Internal.next().next(), SyncTarget::Internal);
        assert_eq!(SyncScope::All.next(), SyncScope::Selected);
        let mut q = SyncQuality::Original;
        for _ in 0..3 {
            q = q.next();
        }
        assert_eq!(q, SyncQuality::Original);
        assert_eq!(SyncQuality::Mp3Medium.mp3_kbps(), Some(192));
        let login = MusicServerRequest::Login {
            server: "https://music.example".into(),
            username: "luca".into(),
            password: "hunter2".into(),
        };
        let shown = format!("{login:?}");
        assert!(shown.contains("music.example") && !shown.contains("hunter2"));
    }

    #[test]
    fn a_choice_picks_playlists_and_belongs_to_one_account() {
        // "Playlists Only" from before reads as Selected Music, every playlist.
        let scope: SyncScope = serde_json::from_str("\"playlists\"").unwrap();
        assert_eq!(scope, SyncScope::Selected);
        let mut c: SyncChoice = serde_json::from_str("{}").unwrap();
        assert_eq!(c, SyncChoice::default());
        assert!(c.includes("p9") && !c.is_empty());
        // Leaving one out of All Playlists keeps the others listed.
        c.toggle("p2", &["p1", "p2", "p3"]);
        assert!(!c.all_playlists && c.includes("p1") && !c.includes("p2") && c.includes("p3"));
        c.toggle("p2", &["p1", "p2", "p3"]);
        assert!(
            c.includes("p2") && !c.includes("p4"),
            "a new playlist waits"
        );
        c.toggle_all();
        assert!(c.all_playlists && c.includes("p4"));
        c.toggle_all();
        assert!(c.is_empty());
        c.favorites = true;
        assert!(!c.is_empty());
        let key = SyncChoice::account_key("https://music.example", "luca");
        let mine = c.for_account(&key);
        assert_eq!(
            (mine.account.as_str(), mine.favorites),
            (key.as_str(), false)
        );
        assert_eq!(mine.for_account(&key), mine);
    }
}
