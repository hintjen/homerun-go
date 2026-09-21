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
    found.sort_unstable_by_key(|l| (l.protocol == Protocol::Udp, l.port));
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
            if protocol == Protocol::Tcp
                && (fields.len() != 5 || !matches!(fields[2], "0.0.0.0:0" | "[::]:0"))
            {
                continue;
            }

            if let Some(port) = port_of(fields[1]) {
                out.push(Listening { protocol, port });
            }
        }
        out
    }

    /// `0.0.0.0:28015`, `[::]:28015`, `127.0.0.1:28016`.
    ///
    /// Split on the last colon: an IPv6 address is full of them.
    fn port_of(address: &str) -> Option<u16> {
        address.rsplit_once(':')?.1.parse().ok()
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
                    port: 28016
                },
                Listening {
                    protocol: Protocol::Tcp,
                    port: 28017
                },
                Listening {
                    protocol: Protocol::Udp,
                    port: 28015
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
                if let Some(port) = local
                    .rsplit_once(':')
                    .and_then(|(_, hex)| u16::from_str_radix(hex, 16).ok())
                {
                    out.push(Listening { protocol, port });
                }
            }
        }
        out
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
            if let Some(port) = name
                .split("->")
                .next()
                .and_then(|local| local.rsplit_once(':'))
                .and_then(|(_, port)| port.parse().ok())
            {
                out.push(Listening { protocol, port });
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
        assert!(
            observed.contains(&Listening {
                protocol: Protocol::Tcp,
                port
            }),
            "bound {port} and saw {observed:?}"
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
            !observed.contains(&Listening {
                protocol: Protocol::Tcp,
                port
            }),
            "reported {port}, which nothing is listening on"
        );
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
