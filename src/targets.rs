//! Target enumeration: which processes can be captured right now.
//!
//! Three sources are merged by process id:
//!
//! * `EnumWindows` for visible top level windows (pid + title + executable),
//! * WASAPI for the audio sessions of the default render endpoint, which tells
//!   whether a process is actually making noise and how loud it is,
//! * SMTC for media metadata (track / artist / playback state).
//!
//! A pid that owns an audio session but no window - a player tucked away in
//! the notification area - still becomes a target, so it can be captured like
//! any other process. Its title comes from SMTC because there is no window to
//! read one from.

use std::collections::{HashMap, HashSet};

use windows::{
    core::{BOOL, Interface, PWSTR},
    Win32::{
        Foundation::{CloseHandle, HWND, LPARAM, MAX_PATH},
        Media::{
            Audio::{
                eMultimedia, eRender, AudioSessionState, AudioSessionStateActive,
                AudioSessionStateExpired, Endpoints::IAudioMeterInformation,
                IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
                MMDeviceEnumerator,
            },
        },
        System::{
            Com::{CoCreateInstance, CLSCTX_ALL},
            Threading::{
                OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
                PROCESS_QUERY_LIMITED_INFORMATION,
            },
        },
        UI::WindowsAndMessaging::{
            EnumWindows, GetWindow, GetWindowLongW, GetWindowTextLengthW, GetWindowTextW,
            GetWindowThreadProcessId, IsWindowVisible, GWL_EXSTYLE, GW_OWNER, WS_EX_TOOLWINDOW,
        },
    },
};

use crate::{
    com::ComGuard,
    media::{self, MediaInfo},
};

/// State of the WASAPI session a process owns.
///
/// The discriminants mirror the `PAC_SESSION_*` constants in the C header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(i32)]
pub enum SessionState {
    /// No audio session at all.
    #[default]
    None = 0,
    /// Rendering audio right now.
    Active = 1,
    /// Has a session but is not rendering.
    Inactive = 2,
    /// The session is being torn down.
    Expired = 3,
}

/// One capturable target.
#[derive(Debug, Clone)]
pub struct TargetInfo {
    pub pid: u32,
    /// Window handle, or 0 when the process has no visible window.
    pub hwnd: u64,
    /// Display title: the window title when there is one, otherwise whatever
    /// SMTC knows, otherwise a placeholder.
    pub title: String,
    /// Executable file name, e.g. `chrome.exe`.
    pub process_name: String,
    /// Full executable path, empty when it could not be queried.
    pub process_path: String,
    pub has_session: bool,
    pub session_state: SessionState,
    /// Live peak of the audio session, 0.0 - 1.0+.
    pub session_peak: f32,
    /// Whether the process currently owns a visible top level window.
    pub has_window: bool,
    pub media: Option<MediaInfo>,
}

/// Result of a full enumeration.
pub struct Enumeration {
    pub targets: Vec<TargetInfo>,
    /// `Err` when the audio session list could not be read, which is normal on
    /// machines without a rendering endpoint. The window list is still valid.
    pub sessions_error: Option<String>,
}

/// A summary of one audio session.
#[derive(Debug, Clone, Copy)]
struct SessionInfo {
    state: AudioSessionState,
    peak: f32,
}

/// Enumerates every process that can be captured, best candidates first.
pub fn enumerate() -> Enumeration {
    let (session_map, sessions_error) = match collect_audio_sessions() {
        Ok(map) => (map, None),
        Err(error) => (HashMap::new(), Some(error)),
    };

    // Media metadata is read once per enumeration and shared by the windows
    // and the session-only entries.
    let sessions = media::list_sessions();

    let mut targets = collect_windows(&session_map, &sessions);
    append_session_only(&mut targets, &session_map, &sessions);
    sort(&mut targets);

    Enumeration { targets, sessions_error }
}

/// Lists the audio sessions of the default render endpoint, keyed by pid.
fn collect_audio_sessions() -> Result<HashMap<u32, SessionInfo>, String> {
    let _guard = ComGuard::new().map_err(|_| "COM could not be initialised".to_string())?;

    let mut map: HashMap<u32, SessionInfo> = HashMap::new();

    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|error| format!("IMMDeviceEnumerator could not be created: {error}"))?;

        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eMultimedia)
            .map_err(|error| {
                format!("the default render endpoint is unavailable (no output device?): {error}")
            })?;

        let manager: IAudioSessionManager2 = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|error| format!("IAudioSessionManager2 could not be activated: {error}"))?;

        let sessions = manager
            .GetSessionEnumerator()
            .map_err(|error| format!("the audio session enumerator is unavailable: {error}"))?;
        let count = sessions
            .GetCount()
            .map_err(|error| format!("the audio session count could not be read: {error}"))?;

        for index in 0..count {
            let Ok(control) = sessions.GetSession(index) else {
                continue;
            };
            let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                continue;
            };
            let Ok(pid) = control2.GetProcessId() else {
                continue;
            };
            if pid == 0 {
                continue;
            }

            let state = control2.GetState().unwrap_or(AudioSessionStateExpired);
            // IAudioMeterInformation is reachable straight from the session.
            let peak = control
                .cast::<IAudioMeterInformation>()
                .and_then(|meter| meter.GetPeakValue())
                .unwrap_or(0.0);

            // A process can own several sessions; keep the liveliest peak and
            // let an active one win over the rest.
            map.entry(pid)
                .and_modify(|existing| {
                    if state == AudioSessionStateActive {
                        existing.state = state;
                    }
                    existing.peak = existing.peak.max(peak);
                })
                .or_insert(SessionInfo { state, peak });
        }
    }

    Ok(map)
}

/// Enumerates visible top level windows and merges in the audio sessions.
fn collect_windows(
    session_map: &HashMap<u32, SessionInfo>,
    sessions: &[MediaInfo],
) -> Vec<TargetInfo> {
    let mut raw: Vec<(HWND, u32, String)> = Vec::new();
    let self_pid = std::process::id();

    unsafe {
        let _ = EnumWindows(Some(enum_window_proc), LPARAM(&mut raw as *mut _ as isize));
    }

    let mut path_cache: HashMap<u32, (String, String)> = HashMap::new();
    let mut out: Vec<TargetInfo> = Vec::new();
    let mut seen_pid: HashMap<u32, usize> = HashMap::new();

    for (hwnd, pid, title) in raw {
        if pid == 0 || pid == self_pid {
            continue;
        }

        // A process may own several windows; keep the one with the longest
        // title, which is usually the informative one.
        if let Some(&slot) = seen_pid.get(&pid) {
            if out[slot].title.len() >= title.len() {
                continue;
            }
            let (process_name, process_path) = path_cache
                .get(&pid)
                .cloned()
                .unwrap_or_else(|| (String::new(), String::new()));
            out[slot] =
                build_entry(hwnd, pid, title, process_name, process_path, session_map, sessions);
            continue;
        }

        let (process_name, process_path) =
            path_cache.entry(pid).or_insert_with(|| query_process_path(pid)).clone();

        seen_pid.insert(pid, out.len());
        out.push(build_entry(
            hwnd,
            pid,
            title,
            process_name,
            process_path,
            session_map,
            sessions,
        ));
    }

    out
}

#[allow(clippy::too_many_arguments)]
fn build_entry(
    hwnd: HWND,
    pid: u32,
    title: String,
    process_name: String,
    process_path: String,
    session_map: &HashMap<u32, SessionInfo>,
    sessions: &[MediaInfo],
) -> TargetInfo {
    let session = session_map.get(&pid);
    TargetInfo {
        pid,
        hwnd: hwnd.0 as u64,
        title,
        media: media::find_by_process(sessions, &process_name),
        process_name,
        process_path,
        has_session: session.is_some(),
        session_state: session.map(|info| session_state_of(info.state)).unwrap_or_default(),
        session_peak: session.map(|info| info.peak).unwrap_or(0.0),
        has_window: true,
    }
}

/// Adds the processes that own an audio session but no visible window.
///
/// Players minimised to the notification area disappear from a window-only
/// list entirely, which makes them impossible to select even though they are
/// happily rendering audio. Their title comes from SMTC, in the same
/// `track - artist` shape players put in their window title.
fn append_session_only(
    out: &mut Vec<TargetInfo>,
    session_map: &HashMap<u32, SessionInfo>,
    sessions: &[MediaInfo],
) {
    let self_pid = std::process::id();
    let known: HashSet<u32> = out.iter().map(|target| target.pid).collect();

    for (&pid, session) in session_map {
        if pid == self_pid || known.contains(&pid) {
            continue;
        }

        let (process_name, process_path) = query_process_path(pid);
        // There is no window title to cross-check against here, so the empty
        // title means "only fall back to the single session on the machine".
        let media = media::resolve_for(sessions, &process_name, "");

        // Plenty of processes keep an empty audio session around; only add the
        // ones that are actually rendering or that SMTC knows about.
        let rendering = session.state == AudioSessionStateActive;
        if !rendering && media.is_none() {
            continue;
        }

        let title = match &media {
            Some(media) if !media.title.is_empty() => {
                if media.artist.is_empty() {
                    media.title.clone()
                } else {
                    format!("{} - {}", media.title, media.artist)
                }
            }
            _ => "(no window: title unavailable)".to_string(),
        };

        out.push(TargetInfo {
            pid,
            hwnd: 0,
            title,
            process_name,
            process_path,
            has_session: true,
            session_state: session_state_of(session.state),
            session_peak: session.peak,
            has_window: false,
            media,
        });
    }
}

/// Orders targets: loudest first, then active sessions, then by name.
fn sort(out: &mut [TargetInfo]) {
    out.sort_by(|a, b| {
        b.session_peak
            .partial_cmp(&a.session_peak)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                (a.session_state != SessionState::Active)
                    .cmp(&(b.session_state != SessionState::Active))
            })
            .then_with(|| a.process_name.to_lowercase().cmp(&b.process_name.to_lowercase()))
            .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
    });
}

fn session_state_of(state: AudioSessionState) -> SessionState {
    if state == AudioSessionStateActive {
        SessionState::Active
    } else if state == AudioSessionStateExpired {
        SessionState::Expired
    } else {
        SessionState::Inactive
    }
}

/// `EnumWindows` callback: keeps visible, titled, non-tool top level windows.
unsafe extern "system" fn enum_window_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let list = &mut *(lparam.0 as *mut Vec<(HWND, u32, String)>);

    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }

    // Drop owned windows (floating toolbars) and tool windows, which keeps the
    // list close to what Alt-Tab shows.
    if !GetWindow(hwnd, GW_OWNER).unwrap_or_default().0.is_null() {
        return BOOL(1);
    }
    let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE);
    if (ex_style & WS_EX_TOOLWINDOW.0 as i32) != 0 {
        return BOOL(1);
    }

    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 {
        return BOOL(1);
    }

    let mut buffer = vec![0u16; (len + 1) as usize];
    let copied = GetWindowTextW(hwnd, &mut buffer);
    if copied <= 0 {
        return BOOL(1);
    }
    let title = String::from_utf16_lossy(&buffer[..copied as usize]).trim().to_string();
    if title.is_empty() {
        return BOOL(1);
    }

    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 {
        return BOOL(1);
    }

    list.push((hwnd, pid, title));
    BOOL(1)
}

/// Reads the executable name and full path of a process.
fn query_process_path(pid: u32) -> (String, String) {
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return (format!("pid:{pid}"), String::new());
        };

        let mut buffer = vec![0u16; MAX_PATH as usize];
        let mut size = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);

        if result.is_err() {
            return (format!("pid:{pid}"), String::new());
        }

        let full = String::from_utf16_lossy(&buffer[..size as usize]);
        let name = full
            .rsplit(['\\', '/'])
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or(&full)
            .to_string();
        (name, full)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Machine probe: prints the merged enumeration so it can be eyeballed
    /// against the task manager.
    #[test]
    fn dumps_targets() {
        let enumeration = enumerate();
        println!("targets: {}", enumeration.targets.len());
        if let Some(error) = &enumeration.sessions_error {
            println!("audio sessions could not be read: {error}");
        }

        for target in enumeration.targets.iter().take(20) {
            println!(
                "  pid={:<7} window={:<5} session={:<5} {:<9} peak={:.3} {:<26} {}",
                target.pid,
                target.has_window,
                target.has_session,
                format!("{:?}", target.session_state),
                target.session_peak,
                target.process_name,
                target.title
            );
            if let Some(media) = &target.media {
                println!(
                    "      media: '{}' / '{}' ({:?})",
                    media.title, media.artist, media.status
                );
            }
        }

        assert!(!enumeration.targets.is_empty(), "nothing was enumerated");
    }
}
