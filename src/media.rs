//! Reads "what is playing right now" from the Windows System Media Transport
//! Controls (SMTC) - the same data the media keys, the volume flyout and the
//! taskbar media card display.
//!
//! SMTC sessions belong to the application, not to a window: a player that
//! lives in the notification area, or that has no window at all, still
//! publishes one. That makes this the reliable source for a display title when
//! `GetWindowText` has nothing to give.

use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
};

/// Playback state of a media session.
///
/// The discriminants mirror the `PAC_MEDIA_*` constants in the C header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(i32)]
pub enum MediaStatus {
    #[default]
    Unknown = 0,
    Playing = 1,
    Paused = 2,
    Stopped = 3,
    Closed = 4,
    Changing = 5,
    Opened = 6,
}

/// One media session published by an application.
#[derive(Debug, Clone, Default)]
pub struct MediaInfo {
    /// AUMID of the owning application. For plain Win32 programs this is
    /// usually the executable file name, sometimes a full path.
    pub app_id: String,
    /// Track or video title.
    pub title: String,
    /// Performer / creator.
    pub artist: String,
    /// Album title.
    pub album: String,
    pub status: MediaStatus,
}

impl MediaInfo {
    /// Whether this session belongs to `process_name`.
    ///
    /// AUMIDs are messy: ordinary Win32 apps publish `cloudmusic.exe` (or a
    /// full path), packaged apps publish something like
    /// `Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic`. So this is a
    /// case-insensitive containment test, guarded by a minimum stem length so
    /// short names cannot match half the system.
    pub fn belongs_to(&self, process_name: &str) -> bool {
        let process = process_name.trim().to_lowercase();
        if process.is_empty() {
            return false;
        }

        let app = self.app_id.to_lowercase();
        let stem = process.strip_suffix(".exe").unwrap_or(&process);
        app.contains(&process) || (stem.len() >= 4 && app.contains(stem))
    }
}

/// Every media session currently published to the system.
///
/// Returns an empty list when SMTC is unavailable: media metadata is a bonus,
/// never a requirement.
pub fn list_sessions() -> Vec<MediaInfo> {
    let Ok(_guard) = crate::com::ComGuard::new() else {
        return Vec::new();
    };

    let Ok(operation) = GlobalSystemMediaTransportControlsSessionManager::RequestAsync() else {
        return Vec::new();
    };
    let Ok(manager) = operation.get() else {
        return Vec::new();
    };
    let Ok(sessions) = manager.GetSessions() else {
        return Vec::new();
    };
    let count = sessions.Size().unwrap_or(0);

    let mut out = Vec::with_capacity(count as usize);
    for index in 0..count {
        let Ok(session) = sessions.GetAt(index) else {
            continue;
        };

        let app_id = session
            .SourceAppUserModelId()
            .map(|id| id.to_string())
            .unwrap_or_default();

        let status = session
            .GetPlaybackInfo()
            .and_then(|info| info.PlaybackStatus())
            .map(status_of)
            .unwrap_or_default();

        // Media properties are read asynchronously; a failure (or an app that
        // never filled them in) just means "no metadata", the session itself
        // stays usable.
        let (title, artist, album) =
            match session.TryGetMediaPropertiesAsync().and_then(|operation| operation.get()) {
                Ok(properties) => (
                    properties.Title().map(|value| value.to_string()).unwrap_or_default(),
                    properties.Artist().map(|value| value.to_string()).unwrap_or_default(),
                    properties.AlbumTitle().map(|value| value.to_string()).unwrap_or_default(),
                ),
                Err(_) => (String::new(), String::new(), String::new()),
            };

        out.push(MediaInfo {
            app_id,
            title: title.trim().to_string(),
            artist: artist.trim().to_string(),
            album: album.trim().to_string(),
            status,
        });
    }

    out
}

/// Picks the session that belongs to `process_name`, without guessing.
pub fn find_by_process(sessions: &[MediaInfo], process_name: &str) -> Option<MediaInfo> {
    sessions.iter().find(|session| session.belongs_to(process_name)).cloned()
}

/// Picks the session that describes what `process_name` is playing.
///
/// Usually that is a plain AUMID match. When the host could not read a window
/// title either (a player tucked away in the notification area) and there is
/// exactly one session on the machine, that session is used instead: there is
/// nothing else to go on, and "one session" cannot be confused with something
/// else that is playing.
pub fn resolve_for(
    sessions: &[MediaInfo],
    process_name: &str,
    window_title: &str,
) -> Option<MediaInfo> {
    if let Some(found) = find_by_process(sessions, process_name) {
        return Some(found);
    }

    if window_title.trim().is_empty() && sessions.len() == 1 {
        let only = &sessions[0];
        if !only.title.trim().is_empty() {
            return Some(only.clone());
        }
    }

    None
}

fn status_of(status: PlaybackStatus) -> MediaStatus {
    if status == PlaybackStatus::Playing {
        MediaStatus::Playing
    } else if status == PlaybackStatus::Paused {
        MediaStatus::Paused
    } else if status == PlaybackStatus::Stopped {
        MediaStatus::Stopped
    } else if status == PlaybackStatus::Closed {
        MediaStatus::Closed
    } else if status == PlaybackStatus::Changing {
        MediaStatus::Changing
    } else {
        MediaStatus::Opened
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(app_id: &str) -> MediaInfo {
        MediaInfo {
            app_id: app_id.to_string(),
            title: "Zero Eclipse".to_string(),
            artist: "Hiroyuki Sawano".to_string(),
            status: MediaStatus::Playing,
            ..Default::default()
        }
    }

    #[test]
    fn matches_aumid_against_process_name() {
        assert!(sample("cloudmusic.exe").belongs_to("cloudmusic.exe"));
        assert!(sample("cloudmusic.exe").belongs_to("CloudMusic.EXE"));
        assert!(!sample("cloudmusic.exe").belongs_to("chrome.exe"));
        assert!(!sample("cloudmusic.exe").belongs_to(""));

        // Full path form.
        assert!(sample(r"C:\Program Files\Netease\CloudMusic\cloudmusic.exe")
            .belongs_to("cloudmusic.exe"));
        // Packaged applications do not carry the process name at all.
        assert!(!sample("Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic")
            .belongs_to("Music.UI.exe"));
    }

    #[test]
    fn falls_back_to_a_single_session_only_without_a_title() {
        let sessions = vec![sample("something-else.exe")];

        assert!(resolve_for(&sessions, "cloudmusic.exe", "some window title").is_none());

        let picked = resolve_for(&sessions, "cloudmusic.exe", "").expect("single session fallback");
        assert_eq!(picked.title, "Zero Eclipse");

        let many = vec![sample("a.exe"), sample("b.exe")];
        assert!(resolve_for(&many, "cloudmusic.exe", "").is_none());
    }

    /// Machine probe: prints every session so the AUMID mapping can be checked
    /// against the running processes.
    #[test]
    fn dumps_sessions() {
        let sessions = list_sessions();
        println!("SMTC sessions: {}", sessions.len());
        for session in &sessions {
            println!(
                "  appId='{}' status={:?} title='{}' artist='{}' album='{}'",
                session.app_id, session.status, session.title, session.artist, session.album
            );
        }
    }
}
