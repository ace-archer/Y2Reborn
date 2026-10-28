//! Navidrome sync on the player: the sign-in check, the sync worker and the
//! automatic daily sync (`reborn_platform::navidrome` does the copying).
//!
//! One sync runs at a time, in its own thread at a lower priority, and
//! answers on a channel; the main loop never waits for it. While it runs the
//! player stays awake and Wi-Fi stays up (the automatic sleep and Wi-Fi idle
//! policies count it as work). It stops when a computer opens the drives or
//! the chosen storage goes away, and the library is scanned once it changed
//! anything. No secret is ever logged.
use crate::Runtime;
use reborn_core::navidrome::{NavidromeRequest, SyncTarget};
use reborn_core::platform::{SdCard, UsbTransfer};
use reborn_core::{Event, MediaSource};
use reborn_observability::Level;
use reborn_platform::navidrome as nd;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The automatic sync runs once the last complete one is this old.
const AUTO_EVERY_S: i64 = 24 * 3600;
/// After an automatic try (done or failed), the next is not before this.
const AUTO_RETRY: Duration = Duration::from_secs(30 * 60);
/// Progress reaches the page at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(500);

pub enum Reply {
    SignedIn(Result<nd::Account, nd::Error>),
    Progress(nd::Progress),
    /// The sync's outcome, and whether it was the automatic one.
    Done(Result<nd::Summary, nd::Error>, bool),
}

struct Running {
    cancel: Arc<AtomicBool>,
    progress: nd::Progress,
    /// Copying to the SD card (inside Reborn's write window).
    on_card: bool,
}

pub struct Navidrome {
    account: Option<nd::Account>,
    account_path: PathBuf,
    signing_in: bool,
    running: Option<Running>,
    /// The storage root the last sync facts were read for, and those facts:
    /// when it last completed (unix) and what it left.
    known_root: Option<PathBuf>,
    synced_at: i64,
    last: Option<(usize, usize)>,
    /// The library is scanned once the scan running now has finished.
    scan_wanted: bool,
    next_auto: Instant,
    replies: (Sender<Reply>, Receiver<Reply>),
}
impl Navidrome {
    pub fn load(root: &Path) -> Self {
        let account_path = nd::account_path(root);
        Self {
            account: nd::load_account(&account_path),
            account_path,
            signing_in: false,
            running: None,
            known_root: None,
            synced_at: 0,
            last: None,
            scan_wanted: false,
            // Not in the first minute: Wi-Fi, the clock and the card settle.
            next_auto: Instant::now() + Duration::from_secs(60),
            replies: channel(),
        }
    }
    /// A sync runs: work that keeps the player awake and online.
    pub fn busy(&self) -> bool {
        self.running.is_some()
    }
}

impl Runtime {
    /// Where synced music goes now: Internal Storage, or the SD card while
    /// one is in and writable.
    fn navidrome_root(&self) -> Option<PathBuf> {
        match self.model.settings.navidrome_target {
            SyncTarget::Internal => Some(self.model.settings.music_directory.clone()),
            SyncTarget::SdCard => {
                let writable = !matches!(
                    self.model.platform.snapshot.storage.sd,
                    SdCard::ReadOnly(_)
                        | SdCard::Unchecked(_)
                        | SdCard::NeedsRepair
                        | SdCard::Error
                        | SdCard::Absent
                );
                self.model
                    .sources
                    .iter()
                    .find(|s| s.online && matches!(s.kind, MediaSource::SdCard(_)))
                    .filter(|_| writable)
                    .map(|s| s.root.clone())
            }
        }
    }
    /// A computer has the drives open: nothing may write to them.
    fn navidrome_usb_busy(&self) -> bool {
        matches!(
            self.model.platform.snapshot.usb,
            UsbTransfer::Ready | UsbTransfer::Copying | UsbTransfer::Starting
        )
    }
    /// What the chosen storage's last sync left, read again when the storage
    /// changed (another choice, a card inserted).
    fn navidrome_known(&mut self) {
        let root = self.navidrome_root();
        if root == self.navidrome.known_root {
            return;
        }
        let manifest = root.as_deref().map(nd::load_manifest).unwrap_or_default();
        let same = self
            .navidrome
            .account
            .as_ref()
            .is_some_and(|a| a.server == manifest.server && a.username == manifest.username);
        self.navidrome.synced_at = if same { manifest.synced_at } else { 0 };
        self.navidrome.last = (same && manifest.synced_at > 0)
            .then_some((manifest.songs.len(), manifest.playlists.len()));
        self.navidrome.known_root = root;
    }
    /// What Settings > Storage > Navidrome shows.
    pub(crate) fn navidrome_view(&mut self) {
        self.navidrome_known();
        let n = &self.navidrome;
        let view = reborn_ui::NavidromeView {
            account: n
                .account
                .as_ref()
                .map(|a| (a.username.clone(), a.host().to_owned())),
            signing_in: n.signing_in,
            running: n
                .running
                .as_ref()
                .map(|r| (r.progress.done, r.progress.total)),
            last: n.last,
            target_missing: n.known_root.is_none(),
        };
        if view != self.ui.navidrome {
            self.ui.navidrome = view;
            self.dirty.mark_render();
        }
    }
    fn navidrome_spawn(
        &self,
        name: &str,
        job: impl FnOnce(Sender<Reply>) + Send + 'static,
    ) -> bool {
        let reply = self.navidrome.replies.0.clone();
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || job(reply))
            .is_ok()
    }
    /// Settings > Storage > Navidrome.
    pub(crate) fn navidrome_request(&mut self, request: NavidromeRequest) -> Result<(), String> {
        let result = match request {
            NavidromeRequest::Login { .. } | NavidromeRequest::SyncNow if !self.online() => {
                Err("Connect to Wi-Fi first".to_owned())
            }
            NavidromeRequest::Login { .. } if self.navidrome.signing_in => Ok(()),
            NavidromeRequest::Login {
                server,
                username,
                password,
            } => match nd::server_address(&server) {
                None => Err("That isn't a server address".to_owned()),
                // Certificate checks need the right date.
                Some(server) if server.starts_with("https") && self.trusted_unix().is_none() => {
                    Err("The clock isn't set yet; try again in a moment".to_owned())
                }
                Some(server) => {
                    let account = nd::Account::new(server, username, &password);
                    self.navidrome.signing_in =
                        self.navidrome_spawn("navidrome-login", move |tx| {
                            let checked = nd::Client::new(account.clone()).ping().map(|_| account);
                            let _ = tx.send(Reply::SignedIn(checked));
                        });
                    Ok(())
                }
            },
            NavidromeRequest::SyncNow => self.navidrome_start(false),
            NavidromeRequest::Cancel => {
                if let Some(r) = &self.navidrome.running {
                    r.cancel.store(true, Ordering::Relaxed);
                }
                Ok(())
            }
            NavidromeRequest::Disconnect => {
                self.navidrome_forget();
                self.ui.flash("Disconnected from Navidrome");
                Ok(())
            }
        };
        self.navidrome_view();
        result
    }
    /// Forget the account (a running sync stops). Synced music stays: it is
    /// the listener's music now, removable like any other.
    pub(crate) fn navidrome_forget(&mut self) {
        if let Some(r) = &self.navidrome.running {
            r.cancel.store(true, Ordering::Relaxed);
        }
        if let Err(e) = nd::forget_account(&self.navidrome.account_path) {
            self.log.emit(
                Level::Warn,
                "navidrome",
                "forget_failed",
                "Navidrome sign-in not removed",
                None,
                json!({"error": e.to_string()}),
            );
        }
        self.navidrome.account = None;
        self.navidrome.known_root = None;
        self.navidrome_view();
    }
    fn navidrome_start(&mut self, automatic: bool) -> Result<(), String> {
        if self.navidrome.running.is_some() {
            return Ok(());
        }
        let account = self
            .navidrome
            .account
            .clone()
            .ok_or("Sign in to Navidrome first")?;
        if self.navidrome_usb_busy() {
            return Err("Unplug from the computer first".into());
        }
        let root = self
            .navidrome_root()
            .ok_or(match self.model.settings.navidrome_target {
                SyncTarget::SdCard => "Insert a working SD card first",
                SyncTarget::Internal => "Internal Storage isn't ready",
            })?;
        let now_unix = self.trusted_unix();
        if account.server.starts_with("https") && now_unix.is_none() {
            return Err("The clock isn't set yet; try again in a moment".into());
        }
        let options = nd::Options {
            scope: self.model.settings.navidrome_scope,
            quality: self.model.settings.navidrome_quality,
            now_unix: now_unix.unwrap_or(0),
            reserve: nd::RESERVE,
        };
        let on_card = self.model.settings.navidrome_target == SyncTarget::SdCard;
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        let started = self.navidrome_spawn("navidrome-sync", move |tx| {
            reborn_platform::lower_thread_priority();
            // The card is read-only between writes (Y2Linux 8.27): the whole
            // sync runs inside Reborn's write window, kept open as it goes.
            let _hold = match on_card
                .then(reborn_platform::client::SdWriteHold::open)
                .transpose()
            {
                Ok(hold) => hold,
                Err(e) => {
                    let refused = nd::Error::Storage(format!("write window: {e}"));
                    let _ = tx.send(Reply::Done(Err(refused), automatic));
                    return;
                }
            };
            let client = nd::Client::new(account);
            let mut sent: Option<Instant> = None;
            let result = nd::sync(&client, &root, &options, &stop, &mut |p| {
                if sent.is_none_or(|at| at.elapsed() >= PROGRESS_EVERY) {
                    sent = Some(Instant::now());
                    let _ = tx.send(Reply::Progress(p));
                }
            });
            let _ = tx.send(Reply::Done(result, automatic));
        });
        if !started {
            return Err("Couldn't start the sync".into());
        }
        self.navidrome.running = Some(Running {
            cancel,
            progress: nd::Progress::default(),
            on_card,
        });
        self.log.emit(
            Level::Info,
            "navidrome",
            "sync_started",
            "Navidrome sync started",
            None,
            json!({
                "automatic": automatic,
                "target": self.model.settings.navidrome_target,
                "scope": self.model.settings.navidrome_scope,
                "quality": self.model.settings.navidrome_quality,
            }),
        );
        Ok(())
    }
    /// Replies, stopping a sync that may no longer write, the scan after it,
    /// and the automatic sync.
    pub(crate) fn navidrome_tick(&mut self) {
        let replies: Vec<Reply> = self.navidrome.replies.1.try_iter().collect();
        for reply in replies {
            self.navidrome_reply(reply);
        }
        if let Some(r) = &self.navidrome.running {
            // (Checked in place: this runs every frame while syncing.)
            let card_gone = self.model.settings.navidrome_target == SyncTarget::SdCard
                && !self
                    .model
                    .sources
                    .iter()
                    .any(|s| s.online && matches!(s.kind, MediaSource::SdCard(_)));
            if card_gone || self.navidrome_usb_busy() {
                r.cancel.store(true, Ordering::Relaxed);
            }
        }
        if self.navidrome.scan_wanted && !self.model.library.scanning {
            self.navidrome.scan_wanted = false;
            self.model.apply(Event::LibraryScanStarted);
            if let Err(error) = self.scanner.scan(self.model.sources.clone()) {
                self.model.apply(Event::LibraryScanFailed(error));
            }
            self.dirty.mark_render();
        }
        let now = Instant::now();
        if !self.model.settings.navidrome_auto
            || self.navidrome.account.is_none()
            || self.navidrome.running.is_some()
            || self.navidrome.signing_in
            || now < self.navidrome.next_auto
            || self.sleep_prepared
            || !self.online()
        {
            return;
        }
        self.navidrome.next_auto = now + AUTO_RETRY;
        // Once a day by the wall clock, so a trusted one is needed.
        let Some(now_unix) = self.trusted_unix() else {
            return;
        };
        self.navidrome_view();
        if now_unix - self.navidrome.synced_at < AUTO_EVERY_S {
            return;
        }
        if let Err(reason) = self.navidrome_start(true) {
            self.log.emit(
                Level::Info,
                "navidrome",
                "auto_skipped",
                "Automatic Navidrome sync not started",
                None,
                json!({"reason": reason}),
            );
        }
        self.navidrome_view();
    }
    fn navidrome_reply(&mut self, reply: Reply) {
        match reply {
            Reply::SignedIn(result) => {
                self.navidrome.signing_in = false;
                match result {
                    Ok(account) => {
                        if let Err(e) = nd::save_account(&self.navidrome.account_path, &account) {
                            self.ui
                                .flash(format!("Couldn't save the Navidrome sign-in: {e}"));
                        } else {
                            self.ui
                                .flash(format!("Signed in to Navidrome as {}", account.username));
                            self.log.emit(
                                Level::Info,
                                "navidrome",
                                "connected",
                                "Navidrome connected",
                                None,
                                json!({"https": account.server.starts_with("https")}),
                            );
                            self.navidrome.account = Some(account);
                            self.navidrome.known_root = None;
                        }
                    }
                    Err(e) => self.ui.flash(e.message()),
                }
            }
            Reply::Progress(p) => {
                if let Some(r) = &mut self.navidrome.running {
                    r.progress = p;
                }
            }
            Reply::Done(result, automatic) => {
                let on_card = self.navidrome.running.take().is_some_and(|r| r.on_card);
                match result {
                    Ok(s) => {
                        self.log.emit(
                            Level::Info,
                            "navidrome",
                            "synced",
                            "Navidrome sync finished",
                            None,
                            json!({
                                "automatic": automatic,
                                "downloaded": s.downloaded,
                                "removed": s.removed,
                                "songs": s.songs,
                                "playlists": s.playlists,
                                "failed": s.failed,
                                "full": s.full,
                            }),
                        );
                        self.navidrome.known_root = None;
                        if s.downloaded > 0 || s.removed > 0 || s.playlists > 0 {
                            self.navidrome.scan_wanted = true;
                        }
                        let said = if s.full {
                            Some("Storage is full; some songs were left out".to_owned())
                        } else if s.failed > 0 {
                            Some(format!(
                                "Synced; {} {} couldn't be copied",
                                s.failed,
                                if s.failed == 1 { "song" } else { "songs" }
                            ))
                        } else if s.downloaded > 0 {
                            Some(format!(
                                "Synced {} new {}",
                                s.downloaded,
                                if s.downloaded == 1 { "song" } else { "songs" }
                            ))
                        } else {
                            // The automatic sync says nothing when nothing changed.
                            (!automatic).then(|| "Navidrome is up to date".to_owned())
                        };
                        if let Some(said) = said {
                            self.ui.flash(said);
                        }
                    }
                    Err(nd::Error::WrongLogin) => {
                        // The password changed on the server: sign in again.
                        self.navidrome_forget();
                        self.ui.flash("Sign in to Navidrome again");
                    }
                    Err(e) => {
                        // Partly copied: what is there now is scanned too.
                        self.navidrome.scan_wanted = true;
                        self.navidrome.known_root = None;
                        self.log.emit(
                            Level::Warn,
                            "navidrome",
                            "sync_failed",
                            "Navidrome sync stopped",
                            None,
                            json!({
                                "automatic": automatic,
                                "error": e.message(),
                                "detail": e.detail(),
                                "on_card": on_card,
                            }),
                        );
                        // Disconnect stopped it and said so already.
                        let disconnected =
                            e == nd::Error::Cancelled && self.navidrome.account.is_none();
                        if !(automatic && e.retry_later()) && !disconnected {
                            self.ui.flash(match e {
                                // As for playlists: a card with errors, or one
                                // being checked, refuses its write window.
                                nd::Error::Storage(_) if on_card => {
                                    "The SD card can't be changed right now. If it has errors, repair it in Settings > Storage.".to_owned()
                                }
                                e => e.message(),
                            });
                        }
                    }
                }
            }
        }
        self.navidrome_view();
    }
}
