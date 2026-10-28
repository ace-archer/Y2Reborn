//! Subsonic sync (Settings > Storage > Subsonic): the choices it keeps and
//! the requests its page sends. The server is the source: songs and
//! playlists are copied to the player, never the other way.
use serde::{Deserialize, Serialize};

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

/// What is synced: the whole library, or only the songs of the playlists
/// (a library larger than the player). Playlists are synced either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncScope {
    #[default]
    All,
    Playlists,
}
impl SyncScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All Music",
            Self::Playlists => "Playlists Only",
        }
    }
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Playlists,
            Self::Playlists => Self::All,
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

/// Settings > Storage > Subsonic requests. A password lives only in
/// `Login` on its way to the one sign-in check and is never printed.
#[derive(Clone, PartialEq, Eq)]
pub enum SubsonicRequest {
    Login {
        server: String,
        username: String,
        password: String,
    },
    SyncNow,
    Cancel,
    Disconnect,
}
impl std::fmt::Debug for SubsonicRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Login { server, .. } => write!(f, "Login {{ server: {server:?}, .. }}"),
            Self::SyncNow => f.write_str("SyncNow"),
            Self::Cancel => f.write_str("Cancel"),
            Self::Disconnect => f.write_str("Disconnect"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_step_round_and_keep_secrets_out_of_debug() {
        assert_eq!(SyncTarget::Internal.next().next(), SyncTarget::Internal);
        assert_eq!(SyncScope::All.next(), SyncScope::Playlists);
        let mut q = SyncQuality::Original;
        for _ in 0..3 {
            q = q.next();
        }
        assert_eq!(q, SyncQuality::Original);
        assert_eq!(SyncQuality::Mp3Medium.mp3_kbps(), Some(192));
        let login = SubsonicRequest::Login {
            server: "https://music.example".into(),
            username: "luca".into(),
            password: "hunter2".into(),
        };
        let shown = format!("{login:?}");
        assert!(shown.contains("music.example") && !shown.contains("hunter2"));
    }
}
