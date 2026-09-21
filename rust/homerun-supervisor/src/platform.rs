//! Every question only an operating system can answer.
//!
//! # Why this module exists at all
//!
//! Windows is the only host for descriptor-driven games, and not just for
//! now: iOS cannot spawn a process, and Android can only exec files shipped
//! inside the APK, so a *downloaded* server binary is unrunnable on both.
//!
//! That would be an argument for writing Windows code inline and being done
//! with it. The reason not to is the suite: this crate builds and tests
//! host-native on any machine in seconds, and that is what makes it worth
//! having. A `#[cfg(windows)]` sprinkled through the fetcher and the engine
//! would mean half of this crate could only be *read* on Linux, and a session
//! developing a game on a Mac could not run the tests that matter.
//!
//! So the OS assumptions live here, behind functions with one meaning each,
//! and everything else in the crate is portable by construction. A session on
//! Linux can probe a game whose server ships for Linux — that is development
//! evidence, never a verdict — and `win32-x64` remains the only platform a
//! verdict is given for.
//!
//! # What this module refuses to do
//!
//! **It never guesses.** Every function here returns what it actually
//! observed, and says nothing rather than inventing a plausible answer. A
//! port list that omits a port the server bound is recoverable — the caller
//! retries — while a port list containing one it did not bind produces a
//! tunnel that connects and carries nothing, which looks exactly like a
//! working server nobody can join.
//!
//! **It reports counters, never rates.** `homerun_core::metrics` turns two
//! samples into a percentage. Computing one here would put that arithmetic
//! back in the place this crate spent a day taking it out of.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use homerun_core::tunnel::Protocol;

/// The host platform string a descriptor declares, for this build.
///
/// Matches `hosts` and `platforms` keys in a `game.json`. The non-Windows
/// values exist so a developer's machine can be described honestly, not
/// because anything ships for them.
pub const HOST: &str = if cfg!(all(windows, target_arch = "x86_64")) {
    "win32-x64"
} else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    "linux-x64"
} else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
    "linux-arm64"
} else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    "darwin-arm64"
} else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
    "darwin-x64"
} else {
    "unknown"
};

/// Whether this is a platform Homerun ships a runner for.
///
/// A session on any other OS can still launch and probe a game — that is what
/// makes local development possible — but what it observes is evidence about
/// that OS and never a verdict for `win32-x64`.
pub fn is_shipping_host() -> bool {
    HOST == "win32-x64"
}

/// A socket a process has open for listening.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listening {
    pub protocol: Protocol,
    pub port: u16,
    /// The local address it is bound to.
    ///
    /// Carried because the port on its own cannot answer the question the
    /// descriptor asks. `expose: false` says a port stays on this computer,
    /// and `127.0.0.1:28016` and `0.0.0.0:28016` are the same port and
    /// opposite answers — the second is an administrative console on the LAN
    /// behind one password. Dropping the address here is what made that
    /// promise unenforceable, so it is kept even though only one caller
    /// reads it.
    pub address: IpAddr,
}

impl Listening {
    /// Whether nothing outside this computer can reach this socket.
    ///
    /// `Ipv6Addr::is_loopback` is false for `::ffff:127.0.0.1`, which is how
    /// a dual-stack listener on loopback appears in some tables — so a
    /// v4-mapped address is unwrapped before the question is asked.
    pub fn is_confined(&self) -> bool {
        match self.address {
            IpAddr::V4(v4) => v4.is_loopback(),
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => v4.is_loopback(),
                None => v6.is_loopback(),
            },
        }
    }
}

/// What a process is costing. Counters, never rates — see the module header.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stats {
    pub rss_kb: u64,
    pub cpu_seconds: f64,
}

/// The ports a process is listening on.
///
/// Empty means "none observed", which for a server that has not finished
/// binding yet is the truthful answer and not an error. Callers poll.
///
/// There is no portable API for this, which is the whole reason this module
/// exists: Windows has `netstat -ano`, Linux has `/proc/net/*`, macOS has
/// `lsof`. Each is parsed below, and each is wrong in its own way if parsed
/// carelessly.
pub fn listening_ports(pid: u32) -> Vec<Listening> {
    let mut found = imp::listening_ports(pid);
    // Sorted so a caller comparing two observations sees a stable order,
    // and deduped because the same socket appears in both the v4 and the v6
    // table when a server binds dual-stack. `tunnel::Protocol` is a wire
    // type with no ordering of its own and does not need one for this.
    found.sort_unstable_by_key(|l| (l.protocol == Protocol::Udp, l.port, l.address));
    found.dedup();
    found
}

/// Resident memory and cumulative CPU for a process.
pub fn process_stats(pid: u32) -> Option<Stats> {
    imp::process_stats(pid)
}

/// Capacity for the doctor's verdict. Zero means unobserved, never unlimited.
/// Probe the nearest existing ancestor when a new install folder does not exist.
pub fn machine_capacity(path: &Path) -> homerun_core::engine::doctor::Machine {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let existing = absolute
        .ancestors()
        .find(|p| p.exists())
        .unwrap_or(Path::new("."));
    let mut machine = homerun_core::engine::doctor::Machine {
        host: HOST.into(),
        ram_mb: 0,
        disk_mb: 0,
        cpu_cores: std::thread::available_parallelism()
            .ok()
            .map(|n| n.get() as u32),
    };
    #[cfg(windows)]
    {
        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command",
                "[Console]::WriteLine((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory); [Console]::WriteLine((Get-Item -LiteralPath $env:HOMERUN_CAPACITY_PATH).PSDrive.Free)"])
            .env("HOMERUN_CAPACITY_PATH", existing).output();
        if let Ok(output) = output {
            let text = String::from_utf8_lossy(&output.stdout);
            let mut lines = text.lines();
            machine.ram_mb = lines
                .next()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0)
                / 1_048_576;
            machine.disk_mb = lines
                .next()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0)
                / 1_048_576;
        }
    }
    #[cfg(unix)]
    {
        if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
            machine.ram_mb = text
                .lines()
                .find(|s| s.starts_with("MemTotal:"))
                .and_then(|s| s.split_whitespace().nth(1))
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0)
                / 1024;
        }
        if let Ok(output) = std::process::Command::new("df")
            .arg("-Pk")
            .arg(existing)
            .output()
        {
            machine.disk_mb = String::from_utf8_lossy(&output.stdout)
                .lines()
                .last()
                .and_then(|s| s.split_whitespace().nth(3))
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0)
                / 1024;
        }
    }
    machine
}

/// Ask a process to stop the way a person pressing Ctrl+C would.
///
/// `Err` on Windows, always, and that is a platform fact rather than a gap to
/// fill in later. A console control event can only be sent to a process group
/// attached to a console, and a server spawned with piped stdio has neither;
/// `GenerateConsoleCtrlEvent` would signal *this* process's group, which
/// includes the app. A game whose only clean stop is an interrupt is
/// therefore unsupported on the one platform that ships, which is why
/// `engine::validate` warns about `stop.via: interrupt` rather than accepting
/// it quietly.
pub fn graceful_interrupt(pid: u32) -> Result<(), String> {
    imp::graceful_interrupt(pid)
}

/// Where a game may write outside its own directory.
///
/// For the probe's stray-write diff: snapshot these before a launch and
/// after, and anything new is a file the backup would miss. A game that keeps
/// saves somewhere in here and cannot be told otherwise does not fit
/// "backup unit = the server directory", which is a finding rather than a
/// bug.
pub fn user_data_roots() -> Vec<PathBuf> {
    imp::user_data_roots()
}

/// The path to an executable inside a directory, spelled for this OS.
///
/// Adds `.exe` on Windows if the descriptor did not. Descriptors name
/// `RustDedicated.exe` today because that is the vendor's own name, but a
/// cross-platform game would name `server`, and the suffix is not something
/// a descriptor should have to carry per platform.
pub fn executable(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    if cfg!(windows) && path.extension().is_none() {
        return path.with_extension("exe");
    }
    path
}

/// Make one directory appear inside another without copying it.
///
/// # Why this exists
///
/// A great many dedicated servers — every Unity one met so far — resolve
/// *both* their game data and their saves relative to the working directory.
/// Their data lives in the runtime directory, which is shared by every server
/// of that game; their saves have to live under the server directory, because
/// that is the unit Homerun backs up, moves and deletes. Satisfy one and the
/// other breaks. See `docs/game-runner.md`.
///
/// So the working directory is the runtime directory, and the save directory
/// the game writes to *inside* it is a link to a real directory under the
/// server. The game sees the layout it insists on; the bytes are where they
/// have to be.
///
/// # A junction on Windows, and not a symbolic link
///
/// `std::os::windows::fs::symlink_dir` needs `SeCreateSymbolicLinkPrivilege`,
/// which a player's account does not have unless Developer Mode is on. A
/// **directory junction** needs no privilege at all, and `std` has no API for
/// one — so this is `DeviceIoControl` with `FSCTL_SET_REPARSE_POINT`, which
/// is the whole reason this module gained a Win32 dependency.
///
/// On Unix it is a symlink, which needs nothing.
pub fn mount_dir(link: &Path, target: &Path) -> Result<(), String> {
    imp::mount_dir(link, target)
}

/// Remove a link made by [`mount_dir`], leaving what it pointed at alone.
///
/// `remove_dir` on a junction removes the junction. `remove_dir_all` is not
/// used here and must not be: the whole point is that the target is a
/// player's save directory.
pub fn unmount_dir(link: &Path) -> Result<(), String> {
    if !is_mount(link) {
        return Ok(());
    }
    #[cfg(windows)]
    let result = std::fs::remove_dir(link);
    #[cfg(unix)]
    let result = std::fs::remove_file(link);
    result.map_err(|e| format!("the link at {} could not be removed: {e}", link.display()))
}

/// Whether this path is a link rather than a real directory.
///
/// `FileType::is_symlink` is true for a junction as well as a symbolic link
/// on Windows, which is what this needs: both are links, and neither is a
/// directory whose contents belong to anybody.
pub fn is_mount(link: &Path) -> bool {
    std::fs::symlink_metadata(link).is_ok_and(|m| m.file_type().is_symlink())
}

/// Where a link points, or `None` if it is not one.
///
/// Used to tell a link left over from *this* server from one left over from a
/// different server of the same game — they share a runtime directory, so a
/// stale link is a launch that would write one world into another's folder.
pub fn mount_target(link: &Path) -> Option<PathBuf> {
    std::fs::read_link(link).ok()
}

/// Held from before fetch until all game effects finish. The file persists;
/// the OS lock, unlike a PID file, is released on abrupt process death too.
pub struct RuntimeLease {
    _file: std::fs::File,
}
impl RuntimeLease {
    pub fn acquire(path: &Path) -> Result<Self, String> {
        if is_mount(path) {
            return Err("The runtime lock cannot be a link.".into());
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0);
        }
        let file = options
            .open(path)
            .map_err(|_| "This runtime is in use or its lock cannot be opened.".to_string())?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: flock only operates on our open file descriptor.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err("This runtime is in use.".into());
            }
        }
        Ok(Self { _file: file })
    }
}

/// Make a file executable, where that is a thing files are.
///
/// A no-op on Windows. On Unix a freshly-unzipped server binary has whatever
/// mode the archive recorded, which for an archive built on Windows is
/// routinely not executable.
pub fn make_executable(path: &Path) -> Result<(), String> {
    imp::make_executable(path)
}

// ─── Windows ────────────────────────────────────────────────────────────────

#[cfg(windows)]
mod imp {
    use super::*;
    use std::process::Command;

    /// `netstat -ano`, which is present on every Windows since XP and needs
    /// no crate and no elevation.
    ///
    /// The parse is positional and the columns differ per protocol, which is
    /// the trap: a TCP row is
    /// `Proto  Local  Foreign  State  PID` and a UDP row is
    /// `Proto  Local  Foreign  PID` — no state column. Reading the pid as
    /// "the fifth field" therefore works for TCP and silently reads
    /// `*:*` for UDP. The pid is taken as the *last* field instead.
    ///
    /// State labels are localized (for example LISTENING / ABHÖREN).
    /// With -a (not -q), a TCP listener has an unspecified foreign endpoint
    /// with port zero. Connected rows have a real foreign endpoint. Identify
    /// listeners by these numeric fields, never the translated state label.
    pub fn listening_ports(pid: u32) -> Vec<Listening> {
        let Ok(output) = Command::new("netstat").args(["-ano"]).output() else {
            return Vec::new();
        };
        parse(&String::from_utf8_lossy(&output.stdout), pid)
    }

    fn parse(text: &str, pid: u32) -> Vec<Listening> {
        let wanted = pid.to_string();
        let mut out = Vec::new();

        for line in text.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            // Proto, local address, foreign address, [state,] pid.
            if fields.len() < 4 {
                continue;
            }
            // The pid is last whatever the protocol -- see the doc comment.
            if *fields.last().unwrap() != wanted {
                continue;
            }

            let protocol = match fields[0].to_ascii_uppercase().as_str() {
                "TCP" => Protocol::Tcp,
                "UDP" => Protocol::Udp,
                _ => continue,
            };

            // A TCP socket that is not listening is a connection this server
            // opened, and publishing one would be publishing an outbound
            // socket. UDP has no state column and every row is a bound port.
            // A localized state label may contain spaces; only the first
            // three fields and the last (PID) have fixed meaning here.
            if protocol == Protocol::Tcp
                && (fields.len() < 5 || !matches!(fields[2], "0.0.0.0:0" | "[::]:0"))
            {
                continue;
            }

            if let Some((address, port)) = endpoint(fields[1]) {
                out.push(Listening {
                    protocol,
                    port,
                    address,
                });
            }
        }
        out
    }

    /// `0.0.0.0:28015`, `[::]:28015`, `127.0.0.1:28016`.
    ///
    /// Split on the last colon: an IPv6 address is full of them, and the
    /// brackets netstat puts around one are not part of it.
    fn endpoint(text: &str) -> Option<(IpAddr, u16)> {
        let (host, port) = text.rsplit_once(':')?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        Some((host.parse().ok()?, port.parse().ok()?))
    }

    #[test]
    fn multiword_state_labels_do_not_hide_listeners() {
        // Synthetic labels, not a claim about any Windows translation.
        let rows = "TCP 127.0.0.1:28016 0.0.0.0:0 STATE LABEL 42\n\
                    TCP [::]:28017 [::]:0 STATE LABEL 42\n\
                    TCP 127.0.0.1:28018 127.0.0.1:50000 OTHER STATE 42\n\
                    TCP 127.0.0.1:28019 0.0.0.0:0 STATE LABEL 43\n\
                    TCP 127.0.0.1:28020 0.0.0.0:0 42";
        assert_eq!(parse(rows, 42), vec![
            Listening { protocol: Protocol::Tcp, port: 28016, address: "127.0.0.1".parse().unwrap() },
            Listening { protocol: Protocol::Tcp, port: 28017, address: "::".parse().unwrap() },
        ], "multiword state labels must retain listeners while excluding connected, wrong-PID and truncated rows");
    }

    #[test]
    fn localized_listeners_exclude_connected_sockets_and_other_pids() {
        let rows = "TCP 127.0.0.1:28016 0.0.0.0:0 ABHÖREN 42\n\
                    TCP [::]:28017 [::]:0 LISTENING 42\n\
                    TCP 127.0.0.1:28018 127.0.0.1:50000 HERGESTELLT 42\n\
                    TCP 127.0.0.1:28019 0.0.0.0:0 ABHÖREN 43\n\
                    UDP 0.0.0.0:28015 *:* 42";
        let found = parse(rows, 42);
        assert_eq!(
            found,
            vec![
                Listening {
                    protocol: Protocol::Tcp,
                    port: 28016,
                    address: "127.0.0.1".parse().unwrap()
                },
                Listening {
                    protocol: Protocol::Tcp,
                    port: 28017,
                    address: "::".parse().unwrap()
                },
                Listening {
                    protocol: Protocol::Udp,
                    port: 28015,
                    address: "0.0.0.0".parse().unwrap()
                },
            ],
            "localized TCP listeners and UDP must be observed without publishing connections"
        );
    }

    /// `Get-Process`, for the two numbers that matter.
    ///
    /// `tasklist` gives working set and no CPU at all; the Win32 calls that
    /// give both need a crate this build does not otherwise want. A
    /// subprocess per sample is affordable because the sampling interval is
    /// tens of seconds, not milliseconds.
    ///
    /// `WorkingSet64` is bytes, and `CPU` is seconds as a floating-point
    /// number in the *current culture* -- which on a machine set to a locale
    /// that writes decimals with a comma prints `12,5`. `-replace` is not
    /// enough to fix that in general, so the value is asked for in the
    /// invariant culture instead.
    pub fn process_stats(pid: u32) -> Option<Stats> {
        let script = format!(
            "$p = Get-Process -Id {pid} -ErrorAction Stop; \
             $cpu = $p.TotalProcessorTime.TotalSeconds.ToString([System.Globalization.CultureInfo]::InvariantCulture); \
             Write-Output \"$($p.WorkingSet64) $cpu\""
        );
        let output = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut fields = text.split_whitespace();
        let bytes: u64 = fields.next()?.parse().ok()?;
        let cpu_seconds: f64 = fields.next()?.parse().ok()?;
        Some(Stats {
            rss_kb: bytes / 1024,
            cpu_seconds,
        })
    }

    pub fn graceful_interrupt(_pid: u32) -> Result<(), String> {
        // See the note on `super::graceful_interrupt`: this is a platform
        // fact, not an unfinished implementation.
        Err(
            "this computer cannot interrupt a server the way Ctrl+C would, so \
             this game has no way to shut down cleanly here."
                .to_string(),
        )
    }

    pub fn user_data_roots() -> Vec<PathBuf> {
        ["APPDATA", "LOCALAPPDATA", "PROGRAMDATA", "USERPROFILE"]
            .iter()
            .filter_map(|key| std::env::var_os(key))
            .map(PathBuf::from)
            .collect()
    }

    pub fn make_executable(_path: &Path) -> Result<(), String> {
        Ok(())
    }

    /// A directory junction, built by hand because `std` has no API for one
    /// and the thing it does have needs a privilege players lack.
    ///
    /// The layout below is `REPARSE_DATA_BUFFER` for
    /// `IO_REPARSE_TAG_MOUNT_POINT`, which is fixed and undocumented in the
    /// helpful sense: an eight-byte header, four `u16` offsets and lengths,
    /// then both spellings of the path one after the other. The lengths
    /// exclude their terminators and the terminators are still required.
    pub fn mount_dir(link: &Path, target: &Path) -> Result<(), String> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        use windows_sys::Win32::System::IO::DeviceIoControl;

        const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;
        const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;

        // The target has to be absolute and fully resolved: a junction stores
        // a path, not a reference, and a relative one would be resolved
        // against whatever the reader's working directory happened to be.
        let full = target.canonicalize().map_err(|e| {
            format!(
                "the folder {} this game's saves belong in could not be found: {e}",
                target.display()
            )
        })?;
        let full = full.to_string_lossy();
        // `canonicalize` gives the verbatim `\\?\C:\…` form; a junction wants
        // the object-manager `\??\C:\…` spelling of the same path.
        let printed = full.strip_prefix(r"\\?\").unwrap_or(&full);
        let substitute = format!(r"\??\{printed}");

        let wide = |text: &str| -> Vec<u16> {
            std::ffi::OsStr::new(text)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect()
        };
        let (substitute, printed) = (wide(&substitute), wide(printed));
        // Lengths exclude the terminator; the buffer still carries it.
        let (sub_bytes, print_bytes) = ((substitute.len() - 1) * 2, (printed.len() - 1) * 2);
        let path_bytes = substitute.len() * 2 + printed.len() * 2;
        if path_bytes + 16 > 16 * 1024 {
            return Err("The save path is too long for a Windows directory junction.".into());
        }

        let mut buffer: Vec<u8> = Vec::with_capacity(8 + 8 + path_bytes);
        buffer.extend_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
        buffer.extend_from_slice(&(((8 + path_bytes) as u16).to_le_bytes()));
        buffer.extend_from_slice(&0u16.to_le_bytes()); // Reserved
        buffer.extend_from_slice(&0u16.to_le_bytes()); // SubstituteNameOffset
        buffer.extend_from_slice(&(sub_bytes as u16).to_le_bytes());
        buffer.extend_from_slice(&((substitute.len() * 2) as u16).to_le_bytes()); // PrintNameOffset
        buffer.extend_from_slice(&(print_bytes as u16).to_le_bytes());
        for unit in substitute.iter().chain(printed.iter()) {
            buffer.extend_from_slice(&unit.to_le_bytes());
        }

        // The link itself is an ordinary empty directory until the reparse
        // point is written onto it.
        std::fs::create_dir(link)
            .map_err(|e| format!("the folder {} could not be created: {e}", link.display()))?;

        let path = wide(&link.to_string_lossy());
        // SAFETY: a NUL-terminated wide path, and the flags a directory needs
        // -- BACKUP_SEMANTICS to open one at all, OPEN_REPARSE_POINT so this
        // opens the link rather than following it.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            let error = std::io::Error::last_os_error();
            let _ = std::fs::remove_dir(link);
            return Err(format!(
                "the folder {} could not be opened: {}",
                link.display(),
                error
            ));
        }

        let mut returned = 0u32;
        // SAFETY: a handle we just opened, and a buffer whose declared length
        // is its own.
        let ok = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_SET_REPARSE_POINT,
                buffer.as_ptr().cast(),
                buffer.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        let failure = std::io::Error::last_os_error();
        // SAFETY: closed exactly once, on both paths.
        unsafe { CloseHandle(handle) };

        if ok == 0 {
            // Leave nothing half-made: an empty directory where a link was
            // meant to be is the shape that makes the next launch think the
            // game's saves are somewhere they are not.
            let _ = std::fs::remove_dir(link);
            return Err(format!(
                "the folder {} could not be linked to {}: {failure}",
                link.display(),
                target.display()
            ));
        }
        Ok(())
    }
}

// ─── Unix ───────────────────────────────────────────────────────────────────

#[cfg(unix)]
mod imp {
    use super::*;

    /// `/proc/net/{tcp,tcp6,udp,udp6}`, matched to the process's own sockets
    /// through `/proc/<pid>/fd`.
    ///
    /// `ss` and `lsof` are easier and neither is guaranteed present; `/proc`
    /// is, on the platform this branch compiles for. The inode is the join:
    /// each `/proc/<pid>/fd/N` that is a socket reads as `socket:[12345]`, and
    /// 12345 appears in the inode column of the tables.
    #[cfg(target_os = "linux")]
    pub fn listening_ports(pid: u32) -> Vec<Listening> {
        let inodes = socket_inodes(pid);
        if inodes.is_empty() {
            return Vec::new();
        }

        let mut out = Vec::new();
        for (file, protocol) in [
            ("tcp", Protocol::Tcp),
            ("tcp6", Protocol::Tcp),
            ("udp", Protocol::Udp),
            ("udp6", Protocol::Udp),
        ] {
            let Ok(text) = std::fs::read_to_string(format!("/proc/net/{file}")) else {
                continue;
            };
            for line in text.lines().skip(1) {
                let fields: Vec<&str> = line.split_whitespace().collect();
                // local_address is field 1, st is 3, inode is 9.
                let (Some(local), Some(state), Some(inode)) =
                    (fields.get(1), fields.get(3), fields.get(9))
                else {
                    continue;
                };
                if !inodes.contains(&inode.to_string()) {
                    continue;
                }
                // 0A is TCP_LISTEN. UDP has no listening state -- a bound
                // socket is 07 (CLOSE) and that is normal.
                if protocol == Protocol::Tcp && *state != "0A" {
                    continue;
                }
                if let Some((address, port)) = endpoint(local) {
                    out.push(Listening {
                        protocol,
                        port,
                        address,
                    });
                }
            }
        }
        out
    }

    /// `/proc/net/*` writes an endpoint as `<address-hex>:<port-hex>`, and
    /// the address half is words of **host** byte order rather than network
    /// order. On every machine this compiles for that is little-endian, so
    /// `0100007F` is `127.0.0.1` and not `1.0.0.127` — reading it the
    /// obvious way gives a plausible address that is not the one bound,
    /// which is exactly the kind of answer this module refuses to produce.
    #[cfg(target_os = "linux")]
    fn endpoint(text: &str) -> Option<(IpAddr, u16)> {
        let (host, port) = text.rsplit_once(':')?;
        let port = u16::from_str_radix(port, 16).ok()?;
        let address = match host.len() {
            8 => IpAddr::from(u32::from_str_radix(host, 16).ok()?.to_le_bytes()),
            32 => {
                let mut bytes = [0u8; 16];
                for (i, word) in host.as_bytes().chunks(8).enumerate() {
                    let word = std::str::from_utf8(word).ok()?;
                    bytes[i * 4..i * 4 + 4]
                        .copy_from_slice(&u32::from_str_radix(word, 16).ok()?.to_le_bytes());
                }
                IpAddr::from(bytes)
            }
            _ => return None,
        };
        Some((address, port))
    }

    #[cfg(target_os = "linux")]
    fn socket_inodes(pid: u32) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .filter_map(|entry| std::fs::read_link(entry.path()).ok())
            .filter_map(|target| {
                let text = target.to_string_lossy().into_owned();
                text.strip_prefix("socket:[")
                    .and_then(|rest| rest.strip_suffix(']'))
                    .map(str::to_string)
            })
            .collect()
    }

    /// macOS has no `/proc`, so `lsof` is the only answer without a crate.
    /// It is a development convenience; nothing ships here.
    #[cfg(not(target_os = "linux"))]
    pub fn listening_ports(pid: u32) -> Vec<Listening> {
        let Ok(output) = std::process::Command::new("lsof")
            .args(["-nP", "-a", "-p", &pid.to_string(), "-i"])
            .output()
        else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&output.stdout);
        let mut out = Vec::new();
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let Some(node) = fields.get(7) else { continue };
            let protocol = match *node {
                "TCP" => Protocol::Tcp,
                "UDP" => Protocol::Udp,
                _ => continue,
            };
            if protocol == Protocol::Tcp && !line.contains("(LISTEN)") {
                continue;
            }
            let Some(name) = fields.get(8) else { continue };
            // `*:28015`, `127.0.0.1:28016`, and for UDP sometimes a trailing
            // ` (Idle)` which `split_whitespace` has already removed.
            if let Some((address, port)) = name.split("->").next().and_then(|local| {
                let (host, port) = local.rsplit_once(':')?;
                // lsof writes the unspecified address as `*`, and brackets an
                // IPv6 one.
                let host = host.trim_start_matches('[').trim_end_matches(']');
                let address: IpAddr = if host == "*" {
                    std::net::Ipv4Addr::UNSPECIFIED.into()
                } else {
                    host.parse().ok()?
                };
                Some((address, port.parse::<u16>().ok()?))
            }) {
                out.push(Listening {
                    protocol,
                    port,
                    address,
                });
            }
        }
        out
    }

    /// Resident memory in KiB from `/proc/<pid>/status`, and cumulative CPU
    /// seconds from `/proc/<pid>/stat`.
    ///
    /// `status` rather than `statm` because it is already in KiB and
    /// labelled, where `statm` is in pages and needs the page size and a
    /// field index to be right.
    #[cfg(target_os = "linux")]
    pub fn process_stats(pid: u32) -> Option<Stats> {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let rss_kb: u64 = status
            .lines()
            .find(|line| line.starts_with("VmRSS:"))?
            .chars()
            .filter(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .ok()?;

        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Field 2 is the executable name in parentheses and may contain both
        // spaces and parentheses, so splitting the whole line is the classic
        // bug here. Everything after the *last* `)` is unambiguous.
        let fields: Vec<&str> = stat.rsplit(')').next()?.split_whitespace().collect();
        // `state` is field 3 and lands at index 0, so field N is at N - 3.
        let utime: u64 = fields.get(11)?.parse().ok()?;
        let stime: u64 = fields.get(12)?.parse().ok()?;

        // The divisor is a property of the kernel rather than a constant. It
        // is 100 everywhere seen so far, which is exactly why it is read:
        // hard-coding it would be right until it silently was not.
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        let ticks = if ticks > 0 { ticks as f64 } else { 100.0 };

        Some(Stats {
            rss_kb,
            cpu_seconds: (utime + stime) as f64 / ticks,
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn process_stats(pid: u32) -> Option<Stats> {
        let output = std::process::Command::new("ps")
            .args(["-o", "rss=,time=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        let mut fields = text.split_whitespace();
        let rss_kb: u64 = fields.next()?.parse().ok()?;
        // `[[dd-]hh:]mm:ss`, most-significant first, however many parts there
        // are.
        let clock = fields.next()?;
        let mut seconds = 0f64;
        for part in clock.replace('-', ":").split(':') {
            seconds = seconds * 60.0 + part.parse::<f64>().ok()?;
        }
        Some(Stats {
            rss_kb,
            cpu_seconds: seconds,
        })
    }

    pub fn graceful_interrupt(pid: u32) -> Result<(), String> {
        // SAFETY: `kill` with a pid this process spawned and a valid signal.
        // The worst a reaped pid can do is return ESRCH.
        let sent = unsafe { libc::kill(pid as i32, libc::SIGINT) };
        if sent == 0 {
            Ok(())
        } else {
            Err("the server did not accept the request to shut down.".to_string())
        }
    }

    pub fn user_data_roots() -> Vec<PathBuf> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut roots = Vec::new();
        if let Some(home) = home {
            for relative in [
                ".config",
                ".local/share",
                ".cache",
                "Library/Application Support",
            ] {
                let path = home.join(relative);
                if path.exists() {
                    roots.push(path);
                }
            }
        }
        roots
    }

    pub fn make_executable(path: &Path) -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt;
        let metadata =
            std::fs::metadata(path).map_err(|_| format!("{} is missing.", path.display()))?;
        let mut permissions = metadata.permissions();
        // Whatever it had, plus execute wherever it is already readable.
        let mode = permissions.mode();
        permissions.set_mode(mode | ((mode & 0o444) >> 2));
        std::fs::set_permissions(path, permissions)
            .map_err(|_| format!("{} could not be made runnable.", path.display()))
    }

    /// A symbolic link, which needs no privilege here — the reason Windows
    /// gets a junction instead is that there it does.
    pub fn mount_dir(link: &Path, target: &Path) -> Result<(), String> {
        let full = target.canonicalize().map_err(|e| {
            format!(
                "the folder {} this game's saves belong in could not be found: {e}",
                target.display()
            )
        })?;
        if let Some(parent) = link.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!("the folder {} could not be created: {e}", parent.display())
            })?;
        }
        std::os::unix::fs::symlink(&full, link).map_err(|e| {
            format!(
                "the folder {} could not be linked to {}: {e}",
                link.display(),
                target.display()
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every platform this crate compiles for has to answer, even if the
    /// answer is "unknown" — a panic here would be a panic at the start of
    /// every launch.
    #[test]
    fn this_build_knows_what_it_is_running_on() {
        assert!(!HOST.is_empty());
        if cfg!(all(windows, target_arch = "x86_64")) {
            assert_eq!(HOST, "win32-x64");
            assert!(is_shipping_host());
        } else {
            assert!(
                !is_shipping_host(),
                "only win32-x64 ships a runner; this build says {HOST}"
            );
        }
    }

    /// The suffix rule, which is why a descriptor does not carry one per
    /// platform.
    #[test]
    fn an_executable_is_spelled_for_this_operating_system() {
        let dir = Path::new("runtime");
        let plain = executable(dir, "server");
        if cfg!(windows) {
            assert_eq!(plain, dir.join("server.exe"));
        } else {
            assert_eq!(plain, dir.join("server"));
        }

        // A descriptor that already said `.exe` -- as the pilot's does,
        // because that is the vendor's own name -- is left alone.
        assert_eq!(
            executable(dir, "RustDedicated.exe"),
            dir.join("RustDedicated.exe")
        );
    }

    /// A pid nothing is using must be silence, not a panic and not an
    /// invented answer. This runs on every platform and is the one case that
    /// is the same everywhere.
    #[test]
    fn a_process_that_does_not_exist_reports_nothing() {
        // Deliberately absurd: pid_max is 4194304 on Linux and Windows pids
        // are multiples of four well below this.
        let nobody = 4_294_967_294u32;
        assert!(process_stats(nobody).is_none());
        assert!(listening_ports(nobody).is_empty());
    }

    /// The process asking is one that certainly exists, so this exercises the
    /// real parse on whatever machine the suite runs on rather than only the
    /// failure path above.
    #[test]
    fn this_very_process_has_measurable_memory() {
        let me = std::process::id();
        let stats = process_stats(me).expect("a running process must be measurable");
        assert!(
            stats.rss_kb > 512,
            "a test binary using {}KiB is not believable",
            stats.rss_kb
        );
        assert!(stats.cpu_seconds >= 0.0);
        assert!(
            stats.cpu_seconds < 86_400.0,
            "{} CPU seconds is not believable",
            stats.cpu_seconds
        );
    }

    /// Counters, never rates: two samples of the same process must not go
    /// backwards. A rate would; a counter cannot.
    #[test]
    fn cpu_and_memory_are_counters_rather_than_rates() {
        let me = std::process::id();
        let first = process_stats(me).expect("measurable");
        // Spin briefly so the second sample has something to have counted.
        let until = std::time::Instant::now() + std::time::Duration::from_millis(30);
        let mut spun = 0u64;
        while std::time::Instant::now() < until {
            spun = spun.wrapping_add(1);
        }
        assert!(spun > 0);
        let second = process_stats(me).expect("measurable");
        assert!(
            second.cpu_seconds >= first.cpu_seconds,
            "{} then {}",
            first.cpu_seconds,
            second.cpu_seconds
        );
    }

    /// The listener this test opens is one this process owns, so it must show
    /// up in this process's own port list. That is the whole contract of
    /// `listening_ports`, and it is testable without a game.
    #[test]
    fn a_port_this_process_is_listening_on_is_observed() {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("loopback must be bindable");
        let port = listener.local_addr().unwrap().port();

        let observed = listening_ports(std::process::id());

        // On a platform whose implementation is a development convenience,
        // the tool it shells out to may simply not be installed. Absence of
        // the whole list is tolerated; a list that exists and is missing this
        // port is not.
        if observed.is_empty() {
            return;
        }
        let found = observed
            .iter()
            .find(|l| l.protocol == Protocol::Tcp && l.port == port)
            .unwrap_or_else(|| panic!("bound {port} and saw {observed:?}"));
        // Bound to loopback above, so this is the address the table has to
        // have reported -- the half of the observation that used to be
        // thrown away, and the only thing that can tell a private port from
        // a published one.
        assert!(
            found.is_confined(),
            "bound 127.0.0.1:{port} and the table says {}",
            found.address
        );
    }

    /// The other half of that contract, and the one that matters more: a port
    /// reported that was never bound produces a tunnel that connects and
    /// carries nothing.
    #[test]
    fn a_port_nothing_is_listening_on_is_not_observed() {
        // Bind, learn the port, drop the listener: now nothing holds it.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };

        let observed = listening_ports(std::process::id());
        assert!(
            !observed
                .iter()
                .any(|l| l.protocol == Protocol::Tcp && l.port == port),
            "reported {port}, which nothing is listening on"
        );
    }

    /// A port bound to every interface must read as *not* confined, which is
    /// the observation the runner refuses a launch on. Testable without a
    /// game, and on every platform.
    #[test]
    fn a_port_bound_to_every_interface_is_not_confined() {
        let listener = std::net::TcpListener::bind("0.0.0.0:0").expect("bindable");
        let port = listener.local_addr().unwrap().port();

        let observed = listening_ports(std::process::id());
        if observed.is_empty() {
            return; // see the note in the test above
        }
        let found = observed
            .iter()
            .find(|l| l.protocol == Protocol::Tcp && l.port == port)
            .unwrap_or_else(|| panic!("bound {port} and saw {observed:?}"));
        assert!(
            !found.is_confined(),
            "bound 0.0.0.0:{port} and it reads as confined at {}",
            found.address
        );
    }

    /// The four shapes the question has to get right, without needing a
    /// socket to produce any of them.
    #[test]
    fn loopback_is_told_from_everything_else_including_a_mapped_address() {
        let confined = |a: &str| {
            Listening {
                protocol: Protocol::Tcp,
                port: 1,
                address: a.parse().unwrap(),
            }
            .is_confined()
        };

        assert!(confined("127.0.0.1"));
        assert!(confined("127.0.0.53"), "all of 127/8 is this computer");
        assert!(confined("::1"));
        // How a dual-stack loopback listener appears in some tables, and the
        // one `Ipv6Addr::is_loopback` answers wrongly on its own.
        assert!(confined("::ffff:127.0.0.1"));

        assert!(!confined("0.0.0.0"));
        assert!(!confined("::"));
        assert!(!confined("192.168.1.10"));
        assert!(!confined("::ffff:192.168.1.10"));
    }

    // ─── one directory appearing inside another ────────────────────────────

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "homerun-mount-{}-{name}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The whole contract in one test: a game writing to the link writes to
    /// the target, and the target is somewhere else entirely.
    ///
    /// On Windows this is a junction rather than a symbolic link, and the
    /// difference is not stylistic — a symbolic link needs a privilege a
    /// player's account does not have, so a test that passed with one would
    /// prove nothing about a player's machine.
    #[test]
    fn a_game_writing_through_a_link_writes_where_the_link_points() {
        let root = scratch("write");
        let target = root.join("server/saves");
        let link = root.join("runtime/server");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(root.join("runtime")).unwrap();

        mount_dir(&link, &target).expect("a link must be makeable without elevation");
        assert!(is_mount(&link), "it must read as a link, not a directory");

        std::fs::write(link.join("world.sav"), "a world").unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("world.sav")).unwrap(),
            "a world",
            "the bytes did not land under the server directory"
        );

        // And removing the link must not be removing the world.
        unmount_dir(&link).unwrap();
        assert!(!link.exists(), "the link outlived its removal");
        assert!(
            target.join("world.sav").exists(),
            "removing the link took the saves with it"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Two servers of one game share a runtime directory, so a link left
    /// behind by one is a launch that would write another's world into it.
    /// Telling them apart needs the target, not just the presence.
    #[test]
    fn a_link_says_where_it_points() {
        let root = scratch("target");
        let (first, second) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let link = root.join("link");

        mount_dir(&link, &first).unwrap();
        let seen = mount_target(&link).expect("a link must say where it points");
        assert_eq!(
            seen.canonicalize().unwrap(),
            first.canonicalize().unwrap(),
            "got {seen:?}"
        );
        assert_ne!(seen.canonicalize().unwrap(), second.canonicalize().unwrap());

        assert!(
            mount_target(&first).is_none(),
            "a real directory is not a link"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Removing something that is not a link is not this function's job, and
    /// silently doing it would delete a player's directory.
    #[test]
    fn a_real_directory_is_never_removed_by_unmounting() {
        let root = scratch("real");
        let real = root.join("not-a-link");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("keep"), "mine").unwrap();

        unmount_dir(&real).expect("unmounting a non-link is not an error");
        assert!(
            real.join("keep").exists(),
            "a real directory was removed by something that only removes links"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn interrupting_nothing_fails_in_a_sentence() {
        let err = graceful_interrupt(4_294_967_294).unwrap_err();
        assert!(err.ends_with('.'), "not a sentence: {err}");
        for forbidden in ["unwrap", "panicked", "errno", "ESRCH"] {
            assert!(!err.contains(forbidden), "{err}");
        }
    }

    /// Used by the probe's stray-write diff, so an empty list would silently
    /// mean "this game wrote nothing outside its directory".
    #[test]
    fn there_is_somewhere_a_game_might_write_outside_its_own_directory() {
        let roots = user_data_roots();
        assert!(
            !roots.is_empty(),
            "the stray-write diff has nothing to compare"
        );
        assert!(roots.iter().all(|r| r.is_absolute()), "{roots:?}");
    }
}
