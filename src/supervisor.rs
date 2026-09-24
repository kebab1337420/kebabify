//! Per-user startup registration and Spotify-to-proxy lifecycle supervision.

use anyhow::{Context, Result, anyhow};
use std::future::Future;
use std::path::Path;
use std::time::Duration;

/// Delay between Spotify and proxy state checks.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "kebabify";

/// Installs or refreshes the current user's `kebabify` autostart command.
#[cfg(windows)]
pub fn install_autostart() -> Result<()> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RegCreateKeyW};

    let command = autostart_command()?;
    let key_path = wide(RUN_KEY);
    let value_name = wide(RUN_VALUE);
    let value_data = wide(&command);
    let mut key = std::ptr::null_mut();

    let status = unsafe { RegCreateKeyW(HKEY_CURRENT_USER, key_path.as_ptr(), &mut key) };
    if status != ERROR_SUCCESS {
        return Err(registry_error("create or open Run key", RUN_KEY, status));
    }

    let result = update_run_value(key, &value_name, &value_data, &command);
    let close_result = close_run_key(key);
    combine_registry_results(result, close_result)
}

/// Removes the current user's `kebabify` autostart value if present.
#[cfg(windows)]
pub fn remove_autostart() -> Result<()> {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_SET_VALUE, RegDeleteValueW, RegOpenKeyExW,
    };

    let key_path = wide(RUN_KEY);
    let value_name = wide(RUN_VALUE);
    let mut key = std::ptr::null_mut();
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            key_path.as_ptr(),
            0,
            KEY_SET_VALUE,
            &mut key,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    if status != ERROR_SUCCESS {
        return Err(registry_error("open Run key", RUN_KEY, status));
    }

    let status = unsafe { RegDeleteValueW(key, value_name.as_ptr()) };
    let result = if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(registry_error("delete kebabify value", RUN_KEY, status))
    };
    let close_result = close_run_key(key);
    combine_registry_results(result, close_result)
}

/// Returns the exact command stored in the current user's Run key.
#[cfg(windows)]
pub fn autostart_command() -> Result<String> {
    let executable = std::env::current_exe().context("Cannot find kebabify executable path")?;
    autostart_command_for(&executable)
}

/// Reports that per-user autostart supervision is unavailable on this platform.
#[cfg(not(windows))]
pub fn install_autostart() -> Result<()> {
    Err(anyhow!(
        "Per-user autostart supervision is only supported on Windows"
    ))
}

/// Reports that there is no per-user autostart value to remove on this platform.
#[cfg(not(windows))]
pub fn remove_autostart() -> Result<()> {
    Ok(())
}

/// Rejects command construction where Windows autostart is unsupported.
#[cfg(not(windows))]
pub fn autostart_command() -> Result<String> {
    Err(anyhow!(
        "Per-user autostart supervision is only supported on Windows"
    ))
}

fn autostart_command_for(executable: &Path) -> Result<String> {
    let executable = executable
        .to_str()
        .context("Kebabify executable path is not valid UTF-8")?;
    Ok(format!("\"{executable}\" supervise"))
}

fn process_name_matches(entry_name: &str, wanted: &str) -> bool {
    entry_name.eq_ignore_ascii_case(wanted)
}

/// Reports whether a Spotify process is currently running.
#[cfg(windows)]
pub fn spotify_running() -> bool {
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    struct Snapshot(HANDLE);

    impl Drop for Snapshot {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
        return false;
    }
    let snapshot = Snapshot(snapshot);
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };

    if unsafe { Process32FirstW(snapshot.0, &mut entry) } == 0 {
        return false;
    }
    loop {
        let name_end = entry
            .szExeFile
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..name_end]);
        if process_name_matches(&name, "spotify.exe") {
            return true;
        }
        if unsafe { Process32NextW(snapshot.0, &mut entry) } == 0 {
            return false;
        }
    }
}

/// Reports that Windows process inspection is unavailable on this platform.
#[cfg(not(windows))]
pub fn spotify_running() -> bool {
    false
}

/// Desired proxy lifecycle transition for one observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkAction {
    /// Spotify is running without its audio proxy.
    StartProxy,
    /// Spotify has exited while its audio proxy remains running.
    StopProxy,
    /// Spotify and the proxy are already in the desired state.
    None,
}

/// Returns the lifecycle transition required for the observed states.
pub fn reconcile(spotify_up: bool, proxy_up: bool) -> LinkAction {
    match (spotify_up, proxy_up) {
        (true, false) => LinkAction::StartProxy,
        (false, true) => LinkAction::StopProxy,
        _ => LinkAction::None,
    }
}

/// Polls Spotify and the proxy until Ctrl+C, applying each required transition.
///
/// Probe failures are treated as a process not being up. Start or stop failures
/// are returned because they indicate that the caller-provided operation could
/// not fulfill a requested transition.
pub async fn supervise<PS, PSFut, PP, PPFut, Start, StartFut, Stop, StopFut>(
    mut probe_spotify: PS,
    mut probe_proxy: PP,
    mut start_proxy: Start,
    mut stop_proxy: Stop,
) -> Result<()>
where
    PS: FnMut() -> PSFut,
    PSFut: Future<Output = Result<bool>>,
    PP: FnMut() -> PPFut,
    PPFut: Future<Output = Result<bool>>,
    Start: FnMut() -> StartFut,
    StartFut: Future<Output = Result<()>>,
    Stop: FnMut() -> StopFut,
    StopFut: Future<Output = Result<()>>,
{
    let shutdown = async { tokio::signal::ctrl_c().await.map_err(anyhow::Error::from) };
    run_supervisor(
        &mut probe_spotify,
        &mut probe_proxy,
        &mut start_proxy,
        &mut stop_proxy,
        shutdown,
        POLL_INTERVAL,
    )
    .await
}

/// Reports whether this platform supports autostart and process watching.
#[cfg(windows)]
pub fn is_supported() -> bool {
    true
}

/// Reports whether this platform supports autostart and process watching.
#[cfg(not(windows))]
pub fn is_supported() -> bool {
    false
}

/// A held single-instance claim. Removing the file on drop keeps a crashed
/// watcher from blocking the next login.
pub struct InstanceLock {
    path: std::path::PathBuf,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Claims the right to run the watcher, or reports that a live one already
/// holds it. The lock file stores a pid, and a file whose pid is dead is
/// treated as stale instead of blocking every future session.
pub fn acquire_instance_lock() -> Result<Option<InstanceLock>> {
    let path =
        lock_path().ok_or_else(|| anyhow!("No application data directory for the watcher lock"))?;
    if let Some(pid) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| content.trim().parse::<u32>().ok())
        && pid != std::process::id()
        && pid_alive(pid)
    {
        return Ok(None);
    }
    let _ = std::fs::remove_file(&path);
    std::fs::create_dir_all(path.parent().unwrap_or_else(|| std::path::Path::new(".")))
        .context("Cannot create the directory for the watcher lock")?;
    std::fs::write(&path, std::process::id().to_string())
        .with_context(|| format!("Cannot write the watcher lock {}", path.display()))?;
    Ok(Some(InstanceLock { path }))
}

fn lock_path() -> Option<std::path::PathBuf> {
    std::env::var_os("APPDATA")
        .map(|appdata| {
            std::path::PathBuf::from(appdata)
                .join("Kebabify")
                .join("supervisor.lock")
        })
        .or_else(|| {
            std::env::var_os("HOME").map(|home| {
                std::path::PathBuf::from(home)
                    .join(".kebabify")
                    .join("supervisor.lock")
            })
        })
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut code) != 0;
        let _ = CloseHandle(handle);
        ok && code == STILL_ACTIVE as u32
    }
}

#[cfg(not(windows))]
fn pid_alive(_pid: u32) -> bool {
    false
}

async fn reconcile_once<PS, PSFut, PP, PPFut, Start, StartFut, Stop, StopFut>(
    probe_spotify: &mut PS,
    probe_proxy: &mut PP,
    start_proxy: &mut Start,
    stop_proxy: &mut Stop,
) -> Result<LinkAction>
where
    PS: FnMut() -> PSFut,
    PSFut: Future<Output = Result<bool>>,
    PP: FnMut() -> PPFut,
    PPFut: Future<Output = Result<bool>>,
    Start: FnMut() -> StartFut,
    StartFut: Future<Output = Result<()>>,
    Stop: FnMut() -> StopFut,
    StopFut: Future<Output = Result<()>>,
{
    let spotify_up = probe_spotify().await.unwrap_or(false);
    let proxy_up = probe_proxy().await.unwrap_or(false);
    let action = reconcile(spotify_up, proxy_up);
    match action {
        LinkAction::StartProxy => start_proxy().await.context("Failed to start audio proxy")?,
        LinkAction::StopProxy => stop_proxy().await.context("Failed to stop audio proxy")?,
        LinkAction::None => {}
    }
    Ok(action)
}

async fn run_supervisor<PS, PSFut, PP, PPFut, Start, StartFut, Stop, StopFut, Shutdown>(
    probe_spotify: &mut PS,
    probe_proxy: &mut PP,
    start_proxy: &mut Start,
    stop_proxy: &mut Stop,
    shutdown: Shutdown,
    poll_interval: Duration,
) -> Result<()>
where
    PS: FnMut() -> PSFut,
    PSFut: Future<Output = Result<bool>>,
    PP: FnMut() -> PPFut,
    PPFut: Future<Output = Result<bool>>,
    Start: FnMut() -> StartFut,
    StartFut: Future<Output = Result<()>>,
    Stop: FnMut() -> StopFut,
    StopFut: Future<Output = Result<()>>,
    Shutdown: Future<Output = Result<()>>,
{
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        reconcile_once(probe_spotify, probe_proxy, start_proxy, stop_proxy).await?;
        tokio::select! {
            biased;
            result = &mut shutdown => return result.context("Failed to listen for Ctrl+C"),
            () = tokio::time::sleep(poll_interval) => {}
        }
    }
}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn registry_error(operation: &str, key: &str, status: u32) -> anyhow::Error {
    anyhow!("Windows registry failed to {operation} for `{key}` with code {status}")
}

#[cfg(windows)]
fn close_run_key(key: windows_sys::Win32::System::Registry::HKEY) -> Result<()> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::RegCloseKey;

    let status = unsafe { RegCloseKey(key) };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(registry_error("close Run key", RUN_KEY, status))
    }
}

#[cfg(windows)]
fn combine_registry_results(result: Result<()>, close_result: Result<()>) -> Result<()> {
    match (result, close_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(close_error)) => Err(anyhow!(
            "{error:#}; additionally failed to close Run key: {close_error:#}"
        )),
    }
}

#[cfg(windows)]
fn update_run_value(
    key: windows_sys::Win32::System::Registry::HKEY,
    value_name: &[u16],
    value_data: &[u16],
    command: &str,
) -> Result<()> {
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{REG_SZ, RegQueryValueExW, RegSetValueExW};

    let mut value_type = 0;
    let mut byte_count = 0_u32;
    let status = unsafe {
        RegQueryValueExW(
            key,
            value_name.as_ptr(),
            std::ptr::null(),
            &mut value_type,
            std::ptr::null_mut(),
            &mut byte_count,
        )
    };
    if status == ERROR_SUCCESS {
        if value_type == REG_SZ {
            let unit_count = usize::try_from(byte_count / size_of::<u16>() as u32)
                .context("Registry value length does not fit in memory")?;
            let mut existing = vec![0_u16; unit_count];
            let mut existing_byte_count = byte_count;
            let status = unsafe {
                RegQueryValueExW(
                    key,
                    value_name.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    existing.as_mut_ptr().cast(),
                    &mut existing_byte_count,
                )
            };
            if status != ERROR_SUCCESS {
                return Err(registry_error("read kebabify value", RUN_KEY, status));
            }
            let end = existing
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(existing.len());
            let existing_command = String::from_utf16_lossy(&existing[..end]);
            if existing_command == command {
                return Ok(());
            }
        }
    } else if status != ERROR_FILE_NOT_FOUND {
        return Err(registry_error("read kebabify value", RUN_KEY, status));
    }

    let byte_count = u32::try_from(std::mem::size_of_val(value_data))
        .context("Kebabify autostart command is too long for the registry")?;
    let status = unsafe {
        RegSetValueExW(
            key,
            value_name.as_ptr(),
            0,
            REG_SZ,
            value_data.as_ptr().cast(),
            byte_count,
        )
    };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(registry_error("write kebabify value", RUN_KEY, status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn autostart_command_quotes_executable_and_appends_argument() {
        let command =
            autostart_command_for(Path::new(r"C:\Program Files\Kebabify\kebabify.exe")).unwrap();
        assert_eq!(
            command,
            r#""C:\Program Files\Kebabify\kebabify.exe" supervise"#
        );
    }

    #[test]
    fn process_names_require_an_exact_case_insensitive_match() {
        assert!(process_name_matches("Spotify.exe", "spotify.exe"));
        assert!(process_name_matches("SPOTIFY.EXE", "spotify.exe"));
        assert!(!process_name_matches("Spotify.exe.bak", "spotify.exe"));
        assert!(!process_name_matches("NotSpotify.exe", "spotify.exe"));
        assert!(!process_name_matches("", "spotify.exe"));
    }

    #[test]
    fn reconcile_covers_every_state_pair() {
        assert_eq!(reconcile(true, false), LinkAction::StartProxy);
        assert_eq!(reconcile(true, true), LinkAction::None);
        assert_eq!(reconcile(false, true), LinkAction::StopProxy);
        assert_eq!(reconcile(false, false), LinkAction::None);
    }

    #[tokio::test]
    async fn supervisor_applies_actions_and_survives_probe_failure() {
        let states = Arc::new(vec![Some(true), Some(true), None, Some(false)]);
        let proxy_up = Arc::new(vec![false, true, false, true]);
        let probe_count = Arc::new(AtomicUsize::new(0));
        let actions = Arc::new(Mutex::new(Vec::new()));

        let probe_states = Arc::clone(&states);
        let probe_count_for_spotify = Arc::clone(&probe_count);
        let mut probe_spotify = move || {
            let probe_states = Arc::clone(&probe_states);
            let probe_count = Arc::clone(&probe_count_for_spotify);
            async move {
                let index = probe_count.fetch_add(1, Ordering::Relaxed);
                probe_states
                    .get(index)
                    .copied()
                    .flatten()
                    .ok_or_else(|| anyhow!("probe error"))
            }
        };

        let proxy_states = Arc::clone(&proxy_up);
        let probe_count_for_proxy = Arc::clone(&probe_count);
        let mut probe_proxy = move || {
            let proxy_states = Arc::clone(&proxy_states);
            let probe_count = Arc::clone(&probe_count_for_proxy);
            async move {
                let index = probe_count.load(Ordering::Relaxed).saturating_sub(1);
                Ok(*proxy_states.get(index).unwrap_or(&false))
            }
        };

        let started_actions = Arc::clone(&actions);
        let mut start_proxy = move || {
            let actions = Arc::clone(&started_actions);
            async move {
                actions.lock().unwrap().push(LinkAction::StartProxy);
                Ok(())
            }
        };
        let stopped_actions = Arc::clone(&actions);
        let mut stop_proxy = move || {
            let actions = Arc::clone(&stopped_actions);
            async move {
                actions.lock().unwrap().push(LinkAction::StopProxy);
                Ok(())
            }
        };

        let shutdown_count = Arc::clone(&probe_count);
        let shutdown = async move {
            while shutdown_count.load(Ordering::Relaxed) < states.len() {
                tokio::task::yield_now().await;
            }
            Ok(())
        };

        run_supervisor(
            &mut probe_spotify,
            &mut probe_proxy,
            &mut start_proxy,
            &mut stop_proxy,
            shutdown,
            Duration::from_millis(1),
        )
        .await
        .unwrap();

        assert_eq!(probe_count.load(Ordering::Relaxed), 4);
        assert_eq!(
            *actions.lock().unwrap(),
            vec![LinkAction::StartProxy, LinkAction::StopProxy]
        );
    }
}
