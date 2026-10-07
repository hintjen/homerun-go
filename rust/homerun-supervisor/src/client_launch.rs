//! Starting a player's own copy of a game: the effects half of
//! `homerun_core::engine::client`, which decides everything.
//!
//! This reads what the core needs off the machine (Steam's install and
//! library files, the Store packages a descriptor names and their two
//! manifests, the running processes), hands it to `client::plan`, runs the
//! launch that comes back, and watches for the game to appear.
//!
//! # Everything that touches the machine is behind [`Probe`]
//!
//! So that the whole flow is tested on any computer against a fake, and the
//! Windows half ([`WindowsProbe`]) is the only part that needs Windows:
//!
//! | Question | How Windows answers it |
//! |---|---|
//! | Where is Steam? | `reg query HKCU\Software\Valve\Steam /v SteamPath` |
//! | Which Store packages? | one `Get-AppxPackage` call for the declared names |
//! | What is running? | a Toolhelp snapshot, and each process's full image path |
//! | Start it | `CreateProcess` on the plan's program, detached |
//!
//! Files are read here with `std`, capped at the size the core's readers
//! accept, and never written.
//!
//! # Install folders are resolved through their links
//!
//! A Game Pass game installed to another drive or folder runs from there, and
//! Windows reports its process from there. Palworld on the machine this was
//! written on runs as
//! `D:\XboxGames\Palworld\Content\Pal\Binaries\WinGDK\Palworld-WinGDK-Shipping.exe`,
//! while the package's install location is
//! `C:\Program Files\WindowsApps\PocketpairInc.Palworld_...`: a junction to
//! `D:\WindowsApps\...`, itself a junction to `D:\XboxGames\Palworld\Content`.
//! Compared as written, the game is never seen, not as started and not as
//! already running. So every install folder is resolved ([`Probe::resolve`])
//! before the core sees it, and Steam libraries too, which can be links of
//! their own.
//!
//! # What "started" means
//!
//! Not the exit code: `explorer.exe` exits 1 having started a Game Pass game,
//! and `steam.exe` hands off to a Steam that is already running and exits.
//! The process list is taken before the launch, and the launch counts once a
//! process the core accepts ([`client::Launch::started`]) appears that was not
//! in it, polled until [`DEFAULT_TIMEOUT`].

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use homerun_core::engine::client::{
    self, join::JoinAddress, manifest, steam, xbox, Launch, Machine, Plan, Process, Program,
};
use homerun_core::engine::descriptor::StoreKind;
use homerun_core::engine::GameDescriptor;

/// How long a launch may take to show a process. Generous on purpose: a
/// Steam that is not running yet starts, signs in and checks for updates
/// before it starts the game.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// How often the process list is read while waiting.
pub const POLL: Duration = Duration::from_millis(500);

/// The largest file read for the core. Matches `manifest::MAX_BYTES`; a
/// `libraryfolders.vdf` is a few KB.
const MAX_FILE: u64 = manifest::MAX_BYTES as u64;

/// Everything this module asks of the machine.
pub trait Probe {
    /// The registry's `SteamPath`, as Steam wrote it. `None` when Steam is not
    /// installed.
    fn steam_path(&self) -> Option<String>;
    /// The installed packages for these package names (the part of a family
    /// name before the `_`), with only Windows' own fields filled in.
    fn packages(&self, names: &[String]) -> Vec<xbox::Package>;
    /// A file's text, or `None` when it is missing, unreadable or too large.
    fn read(&self, path: &Path) -> Option<String>;
    /// Every running process whose image path could be read.
    fn processes(&self) -> Vec<Process>;
    /// `%SystemRoot%`, for `explorer.exe`.
    fn system_root(&self) -> Option<PathBuf>;
    /// Where a folder really is, every junction and link followed, as a plain
    /// drive path. `None` when it cannot be resolved; the caller keeps the
    /// path it had.
    fn resolve(&self, path: &str) -> Option<String>;
    /// Start `program` detached, with exactly these arguments.
    fn spawn(&self, program: &Path, args: &[String]) -> Result<(), String>;
}

/// What happened, for the caller to report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The plan was not a launch: already running, not installed, no stores.
    Planned(Plan),
    /// The launch ran and the game appeared.
    Started { launch: Launch, pid: u32 },
    /// The launch ran and no game appeared in time.
    NotStarted { launch: Launch },
    /// The launch could not be run at all.
    Failed { launch: Launch, message: String },
}

/// Read the machine for this descriptor.
///
/// Only what the descriptor declares is looked at: Steam's libraries for its
/// app id, and the Store packages for its families.
pub fn survey(descriptor: &GameDescriptor, probe: &dyn Probe) -> Machine {
    let stores = &descriptor.client.stores;
    let steam_ids: Vec<u32> = stores
        .iter()
        .filter(|s| s.store == StoreKind::Steam)
        .filter_map(|s| s.app_id)
        .collect();
    let families: Vec<&str> = stores
        .iter()
        .filter(|s| s.store == StoreKind::Xbox)
        .filter_map(|s| s.package_family_name.as_deref())
        .filter(|f| xbox::is_package_family_name(f))
        .collect();

    Machine {
        steam: if steam_ids.is_empty() {
            None
        } else {
            survey_steam(&steam_ids, probe)
        },
        xbox: if families.is_empty() {
            Vec::new()
        } else {
            survey_xbox(&families, probe)
        },
        processes: probe.processes(),
    }
}

fn survey_steam(app_ids: &[u32], probe: &dyn Probe) -> Option<steam::SteamFound> {
    let root = probe.steam_path()?;
    let root = root.trim_end_matches(['\\', '/']).replace('/', "\\");
    let exe = format!("{root}\\steam.exe");

    // Steam's own folder is a library even when the file does not say so,
    // which the older format never did.
    let mut libraries = probe
        .read(
            &Path::new(&root)
                .join("steamapps")
                .join("libraryfolders.vdf"),
        )
        .and_then(|text| steam::parse_library_folders(&text).ok())
        .unwrap_or_default();
    if !libraries.iter().any(|l| {
        l.path
            .trim_end_matches(['\\', '/'])
            .eq_ignore_ascii_case(&root)
    }) {
        libraries.push(steam::Library {
            path: root.clone(),
            apps: None,
        });
    }

    let mut installed = Vec::new();
    for app_id in app_ids {
        for library in libraries.iter().filter(|l| l.may_hold(*app_id)) {
            let Some(text) = probe.read(Path::new(&library.manifest_path(*app_id))) else {
                continue;
            };
            if let Ok(manifest) = steam::parse_app_manifest(&text) {
                if manifest.app_id == *app_id {
                    installed.push(steam::Installed {
                        library: probe
                            .resolve(&library.path)
                            .unwrap_or_else(|| library.path.clone()),
                        manifest,
                    });
                    break;
                }
            }
        }
    }
    Some(steam::SteamFound { exe, installed })
}

fn survey_xbox(families: &[&str], probe: &dyn Probe) -> Vec<xbox::Package> {
    let names: Vec<String> = families
        .iter()
        .filter_map(|f| f.rsplit_once('_').map(|(name, _)| name.to_string()))
        .collect();
    probe
        .packages(&names)
        .into_iter()
        .filter(|p| {
            families
                .iter()
                .any(|f| f.eq_ignore_ascii_case(&p.package_family_name))
        })
        .map(|mut p| {
            if let Some(real) = probe.resolve(&p.install_location) {
                p.install_location = real;
            }
            let dir = PathBuf::from(&p.install_location);
            p.applications = probe
                .read(&dir.join("AppxManifest.xml"))
                .and_then(|text| manifest::appx_applications(&text).ok())
                .unwrap_or_default();
            p.executables = probe
                .read(&dir.join("MicrosoftGame.config"))
                .and_then(|text| manifest::game_executables(&text).ok())
                .unwrap_or_default();
            p
        })
        .collect()
}

/// Survey, plan, and if the plan is a launch, run it and wait for the game.
///
/// `on_plan` is told the plan before anything is started, so a caller can
/// show the join modal while the game is still loading.
pub fn launch(
    descriptor: &GameDescriptor,
    probe: &dyn Probe,
    address: Option<&JoinAddress>,
    timeout: Duration,
    poll: Duration,
    on_plan: &mut dyn FnMut(&Plan),
) -> Outcome {
    let machine = survey(descriptor, probe);
    let plan = client::plan(descriptor, &machine, address);
    on_plan(&plan);
    let launch = match plan {
        Plan::Launch(launch) => launch,
        other => return Outcome::Planned(other),
    };

    let program = match &launch.program {
        Program::Steam { exe } => PathBuf::from(exe),
        Program::Explorer => match probe.system_root() {
            Some(root) => root.join("explorer.exe"),
            None => {
                return Outcome::Failed {
                    launch,
                    message: "Homerun could not find Windows' own folder to start the game from."
                        .into(),
                }
            }
        },
    };

    let before = machine.processes;
    if let Err(message) = probe.spawn(&program, &launch.args) {
        return Outcome::Failed { launch, message };
    }

    let deadline = Instant::now() + timeout;
    loop {
        if let Some(pid) = launch.started(&before, &probe.processes()) {
            return Outcome::Started { launch, pid };
        }
        if Instant::now() >= deadline {
            return Outcome::NotStarted { launch };
        }
        std::thread::sleep(poll);
    }
}

/// Read a file for the core: missing, unreadable or over the cap is `None`.
/// Lossy, since a stray byte in a file a game's author wrote should cost that
/// value, not the whole read.
pub fn read_capped(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE {
        return None;
    }
    std::fs::read(path)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// A folder's real location: `canonicalize` when the folder can be opened,
/// and otherwise each link followed by hand. The second matters because
/// `C:\Program Files\WindowsApps` lets a user read a junction's target but
/// may refuse opening what is behind it, and then `canonicalize` fails.
pub fn resolve_links(path: &Path) -> Option<String> {
    if let Ok(real) = std::fs::canonicalize(path) {
        return Some(plain_path(&real.to_string_lossy()));
    }
    let mut current = path.to_path_buf();
    let mut followed = false;
    // A loop of links is not a folder: give up rather than spin.
    for _ in 0..8 {
        match std::fs::read_link(&current) {
            Ok(target) => {
                current = PathBuf::from(plain_path(&target.to_string_lossy()));
                followed = true;
            }
            Err(_) => break,
        }
    }
    followed.then(|| plain_path(&current.to_string_lossy()))
}

/// `\\?\D:\x\` -> `D:\x`, and `\\?\UNC\server\share` -> `\\server\share`:
/// the spelling Windows reports processes in, so the two compare.
pub fn plain_path(path: &str) -> String {
    let path = if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(path).to_string()
    };
    let trimmed = path.trim_end_matches(['\\', '/']);
    // A drive root keeps its separator: `D:\`, not `D:`.
    if trimmed.len() == 2 && trimmed.ends_with(':') {
        format!("{trimmed}\\")
    } else {
        trimmed.to_string()
    }
}

/// The machine, as Windows describes it.
pub struct WindowsProbe;

#[cfg(windows)]
impl Probe for WindowsProbe {
    fn steam_path(&self) -> Option<String> {
        let output = hidden("reg")
            .args(["query", r"HKCU\Software\Valve\Steam", "/v", "SteamPath"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        parse_reg_query(&String::from_utf8_lossy(&output.stdout), "SteamPath")
    }

    fn packages(&self, names: &[String]) -> Vec<xbox::Package> {
        // The names go in through the environment, never into the script's
        // text, so nothing in one is ever PowerShell. They are also held to a
        // package name's shape first.
        let names: Vec<&str> = names
            .iter()
            .map(String::as_str)
            .filter(|n| {
                !n.is_empty()
                    && n.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
            })
            .collect();
        if names.is_empty() {
            return Vec::new();
        }
        const SCRIPT: &str = "$ErrorActionPreference = 'SilentlyContinue'; \
             $found = foreach ($n in ($env:HOMERUN_PACKAGE_NAMES -split ',')) { Get-AppxPackage -Name $n }; \
             ConvertTo-Json -Compress -InputObject @($found | ForEach-Object { [ordered]@{ \
               packageFamilyName = $_.PackageFamilyName; \
               installLocation = \"$($_.InstallLocation)\"; \
               signatureKind = \"$($_.SignatureKind)\"; \
               isFramework = [bool]$_.IsFramework } })";
        let Ok(output) = hidden("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
            .env("HOMERUN_PACKAGE_NAMES", names.join(","))
            .output()
        else {
            return Vec::new();
        };
        parse_packages(&String::from_utf8_lossy(&output.stdout))
    }

    fn read(&self, path: &Path) -> Option<String> {
        read_capped(path)
    }

    fn resolve(&self, path: &str) -> Option<String> {
        resolve_links(Path::new(path))
    }

    fn processes(&self) -> Vec<Process> {
        win::processes()
    }

    fn system_root(&self) -> Option<PathBuf> {
        std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    }

    fn spawn(&self, program: &Path, args: &[String]) -> Result<(), String> {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
        // Detached and in a group of its own: the game, or the Steam it starts,
        // must not be tied to this short-lived runner or to its console.
        std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
            .spawn()
            .map(|_| ())
            .map_err(|_| "Homerun could not start the game's launcher.".to_string())
    }
}

#[cfg(not(windows))]
impl Probe for WindowsProbe {
    fn steam_path(&self) -> Option<String> {
        None
    }
    fn packages(&self, _names: &[String]) -> Vec<xbox::Package> {
        Vec::new()
    }
    fn read(&self, path: &Path) -> Option<String> {
        read_capped(path)
    }
    fn processes(&self) -> Vec<Process> {
        Vec::new()
    }
    fn system_root(&self) -> Option<PathBuf> {
        None
    }
    fn resolve(&self, path: &str) -> Option<String> {
        resolve_links(Path::new(path))
    }
    fn spawn(&self, _program: &Path, _args: &[String]) -> Result<(), String> {
        Err("Starting a game from Steam or Game Pass works only on Windows.".into())
    }
}

/// A command with no console window of its own.
#[cfg(windows)]
fn hidden(program: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
    let mut command = std::process::Command::new(program);
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

/// The value of `name` in `reg query` output:
/// `    SteamPath    REG_SZ    c:/program files (x86)/steam`.
pub fn parse_reg_query(output: &str, name: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let line = line.trim();
        let rest = line.strip_prefix(name)?;
        let (kind, value) = rest.trim_start().split_once(char::is_whitespace)?;
        (kind.starts_with("REG_") && !value.trim().is_empty()).then(|| value.trim().to_string())
    })
}

/// `Get-AppxPackage`'s JSON, as the script above writes it. Anything that is
/// not that shape is no packages.
pub fn parse_packages(json: &str) -> Vec<xbox::Package> {
    serde_json::from_str::<Vec<xbox::Package>>(json.trim()).unwrap_or_default()
}

#[cfg(windows)]
mod win {
    use homerun_core::engine::client::Process;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    /// Every process whose full image path this user may read. Limited
    /// query rights are enough for a process of the same user, which a game
    /// the player started is; a system process that refuses is left out.
    pub fn processes() -> Vec<Process> {
        let mut out = Vec::new();
        // SAFETY: a plain snapshot handle, closed below on every path.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        // SAFETY: `entry` is sized as the API requires.
        let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
        while ok {
            if let Some(path) = image_path(entry.th32ProcessID) {
                out.push(Process {
                    pid: entry.th32ProcessID,
                    path,
                });
            }
            // SAFETY: as above.
            ok = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
        }
        // SAFETY: opened above, closed once.
        unsafe { CloseHandle(snapshot) };
        out
    }

    fn image_path(pid: u32) -> Option<String> {
        if pid == 0 {
            return None;
        }
        // SAFETY: a query-only handle, closed below.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }
        let mut buffer = [0u16; 32_768];
        let mut len = buffer.len() as u32;
        // SAFETY: `len` is the buffer's capacity in u16s, as the API requires.
        let ok = unsafe {
            QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut len)
        } != 0;
        // SAFETY: opened above, closed once.
        unsafe { CloseHandle(handle) };
        ok.then(|| String::from_utf16_lossy(&buffer[..len as usize]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use homerun_core::engine::descriptor::{Client, ClientStore, JoinVia};
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    const XBOX_DIR: &str =
        r"C:\Program Files\WindowsApps\PocketpairInc.Palworld_1.10.2999.0_x64__ad4psfrxyesvt";
    const XBOX_GAME: &str = r"C:\Program Files\WindowsApps\PocketpairInc.Palworld_1.10.2999.0_x64__ad4psfrxyesvt\Pal\Binaries\WinGDK\Palworld-WinGDK-Shipping.exe";
    const STEAM_GAME: &str =
        r"D:\SteamLibrary\steamapps\common\Palworld\Pal\Binaries\Win64\Palworld-Win64-Shipping.exe";

    /// A machine in memory. `appear` is added to the process list after the
    /// first spawn, after `delay` more reads of it.
    #[derive(Default)]
    struct Fake {
        steam_path: Option<String>,
        packages: Vec<xbox::Package>,
        files: BTreeMap<String, String>,
        processes: RefCell<Vec<Process>>,
        appear: Option<Process>,
        delay: RefCell<u32>,
        spawned: RefCell<Vec<(PathBuf, Vec<String>)>>,
        spawn_fails: bool,
        asked_names: RefCell<Vec<String>>,
        /// Folder -> where it really is.
        links: BTreeMap<String, String>,
    }

    impl Probe for Fake {
        fn steam_path(&self) -> Option<String> {
            self.steam_path.clone()
        }
        fn packages(&self, names: &[String]) -> Vec<xbox::Package> {
            self.asked_names.borrow_mut().extend(names.iter().cloned());
            self.packages.clone()
        }
        fn read(&self, path: &Path) -> Option<String> {
            self.files
                .get(&path.to_string_lossy().replace('/', "\\"))
                .cloned()
        }
        fn processes(&self) -> Vec<Process> {
            if !self.spawned.borrow().is_empty() {
                if let Some(p) = &self.appear {
                    let mut delay = self.delay.borrow_mut();
                    if *delay == 0 {
                        let mut list = self.processes.borrow_mut();
                        if !list.contains(p) {
                            list.push(p.clone());
                        }
                    } else {
                        *delay -= 1;
                    }
                }
            }
            self.processes.borrow().clone()
        }
        fn system_root(&self) -> Option<PathBuf> {
            Some(PathBuf::from(r"C:\Windows"))
        }
        fn resolve(&self, path: &str) -> Option<String> {
            self.links.get(path).cloned()
        }
        fn spawn(&self, program: &Path, args: &[String]) -> Result<(), String> {
            if self.spawn_fails {
                return Err("no".into());
            }
            self.spawned
                .borrow_mut()
                .push((program.to_path_buf(), args.to_vec()));
            Ok(())
        }
    }

    fn palworld() -> GameDescriptor {
        GameDescriptor {
            id: "palworld".into(),
            client: Client {
                stores: vec![
                    ClientStore {
                        store: StoreKind::Steam,
                        app_id: Some(1623730),
                        ..Default::default()
                    },
                    ClientStore {
                        store: StoreKind::Xbox,
                        package_family_name: Some("PocketpairInc.Palworld_ad4psfrxyesvt".into()),
                        application_id: Some("AppPalShipping".into()),
                        join: JoinVia::Info,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn game_pass() -> Fake {
        let mut f = Fake {
            packages: vec![xbox::Package {
                package_family_name: "PocketpairInc.Palworld_ad4psfrxyesvt".into(),
                install_location: XBOX_DIR.into(),
                signature_kind: "Store".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        f.files.insert(
            format!(r"{XBOX_DIR}\AppxManifest.xml"),
            include_str!("../../homerun-core/src/engine/testdata/palworld.AppxManifest.xml").into(),
        );
        f.files.insert(
            format!(r"{XBOX_DIR}\MicrosoftGame.config"),
            include_str!("../../homerun-core/src/engine/testdata/palworld.MicrosoftGame.config")
                .into(),
        );
        f
    }

    fn steam() -> Fake {
        let mut f = Fake {
            steam_path: Some("c:/program files (x86)/steam".into()),
            ..Default::default()
        };
        f.files.insert(
            r"c:\program files (x86)\steam\steamapps\libraryfolders.vdf".into(),
            "\"libraryfolders\" { \"0\" { \"path\" \"c:\\\\program files (x86)\\\\steam\" \"apps\" { } } \
             \"1\" { \"path\" \"D:\\\\SteamLibrary\" \"apps\" { \"1623730\" \"1\" } } }"
                .into(),
        );
        f.files.insert(
            r"D:\SteamLibrary\steamapps\appmanifest_1623730.acf".into(),
            "\"AppState\" { \"appid\" \"1623730\" \"installdir\" \"Palworld\" \"StateFlags\" \"4\" }".into(),
        );
        f
    }

    fn run(fake: &Fake) -> (Outcome, Vec<Plan>) {
        let mut plans = Vec::new();
        let outcome = launch(
            &palworld(),
            fake,
            None,
            Duration::from_millis(50),
            Duration::ZERO,
            &mut |p| plans.push(p.clone()),
        );
        (outcome, plans)
    }

    #[test]
    fn game_pass_is_surveyed_from_its_manifests_and_started_through_explorer() {
        let mut fake = game_pass();
        fake.appear = Some(Process {
            pid: 77,
            path: XBOX_GAME.into(),
        });
        let (outcome, plans) = run(&fake);

        assert_eq!(*fake.asked_names.borrow(), ["PocketpairInc.Palworld"]);
        assert!(
            matches!(plans[..], [Plan::Launch(_)]),
            "told before anything starts"
        );
        assert_eq!(
            *fake.spawned.borrow(),
            [(
                PathBuf::from(r"C:\Windows\explorer.exe"),
                vec![
                    r"shell:AppsFolder\PocketpairInc.Palworld_ad4psfrxyesvt!AppPalShipping"
                        .to_string()
                ]
            )]
        );
        let Outcome::Started { launch, pid } = outcome else {
            panic!("{outcome:?}")
        };
        assert_eq!(pid, 77);
        assert_eq!(launch.executable.as_deref(), Some(XBOX_GAME));
    }

    /// The case that was seen: Game Pass installed to `D:\XboxGames`, so the
    /// package's install location is two junctions away from where the game
    /// runs, and Windows reports the process from where it runs.
    #[test]
    fn a_game_installed_through_junctions_is_seen_where_it_really_runs() {
        const REAL: &str = r"D:\XboxGames\Palworld\Content";
        let real_game = format!(r"{REAL}\Pal\Binaries\WinGDK\Palworld-WinGDK-Shipping.exe");

        let mut fake = game_pass();
        fake.links.insert(XBOX_DIR.into(), REAL.into());
        // The manifests read the same through either path.
        let files: Vec<_> = fake.files.clone().into_iter().collect();
        for (path, text) in files {
            fake.files.insert(path.replace(XBOX_DIR, REAL), text);
        }
        fake.appear = Some(Process {
            pid: 250836,
            path: real_game.clone(),
        });
        let (outcome, _) = run(&fake);
        let Outcome::Started { launch, pid } = outcome else {
            panic!("{outcome:?}")
        };
        assert_eq!(pid, 250836);
        assert_eq!(launch.executable.as_deref(), Some(real_game.as_str()));
        assert_eq!(launch.install_dir, REAL);

        // And a copy already running from there is already running.
        let mut fake = game_pass();
        fake.links.insert(XBOX_DIR.into(), REAL.into());
        let files: Vec<_> = fake.files.clone().into_iter().collect();
        for (path, text) in files {
            fake.files.insert(path.replace(XBOX_DIR, REAL), text);
        }
        fake.processes.borrow_mut().push(Process {
            pid: 1,
            path: real_game,
        });
        assert!(matches!(
            run(&fake).0,
            Outcome::Planned(Plan::AlreadyRunning { pid: 1, .. })
        ));
    }

    #[test]
    fn extended_paths_are_written_the_way_processes_are_reported() {
        assert_eq!(
            plain_path(r"\\?\D:\XboxGames\Palworld\Content\"),
            r"D:\XboxGames\Palworld\Content"
        );
        assert_eq!(
            plain_path(r"\\?\UNC\server\share\games"),
            r"\\server\share\games"
        );
        assert_eq!(plain_path(r"\\?\D:\"), r"D:\");
        assert_eq!(plain_path(r"D:\Games"), r"D:\Games");
    }

    #[test]
    fn the_launch_stub_alone_is_not_started() {
        let mut fake = game_pass();
        fake.appear = Some(Process {
            pid: 5,
            path: format!(r"{XBOX_DIR}\GameLaunchHelper.exe"),
        });
        assert!(matches!(run(&fake).0, Outcome::NotStarted { .. }));
    }

    #[test]
    fn steam_is_found_through_its_library_file_and_wins() {
        let mut fake = steam();
        fake.packages = game_pass().packages;
        fake.files.extend(game_pass().files);
        fake.appear = Some(Process {
            pid: 9,
            path: STEAM_GAME.into(),
        });
        fake.delay = RefCell::new(2);
        let (outcome, _) = run(&fake);

        assert_eq!(
            *fake.spawned.borrow(),
            [(
                PathBuf::from(r"c:\program files (x86)\steam\steam.exe"),
                vec![
                    "-silent".to_string(),
                    "steam://rungameid/1623730".to_string()
                ]
            )]
        );
        assert!(
            matches!(outcome, Outcome::Started { pid: 9, .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn steams_own_folder_is_searched_when_the_library_file_is_missing() {
        let mut fake = Fake {
            steam_path: Some(r"C:\Steam".into()),
            ..Default::default()
        };
        fake.files.insert(
            r"C:\Steam\steamapps\appmanifest_1623730.acf".into(),
            "\"AppState\" { \"appid\" \"1623730\" \"installdir\" \"Palworld\" }".into(),
        );
        let machine = survey(&palworld(), &fake);
        assert_eq!(
            machine.steam.unwrap().installed[0].install_dir(),
            r"C:\Steam\steamapps\common\Palworld"
        );
    }

    #[test]
    fn a_lingering_copy_starts_nothing() {
        let fake = game_pass();
        fake.processes.borrow_mut().push(Process {
            pid: 180936,
            path: XBOX_GAME.into(),
        });
        let (outcome, _) = run(&fake);
        assert!(matches!(
            outcome,
            Outcome::Planned(Plan::AlreadyRunning { pid: 180936, .. })
        ));
        assert!(fake.spawned.borrow().is_empty());
    }

    #[test]
    fn nothing_installed_starts_nothing() {
        let fake = Fake::default();
        assert!(matches!(
            run(&fake).0,
            Outcome::Planned(Plan::NotInstalled { .. })
        ));
        assert!(fake.spawned.borrow().is_empty());
        assert!(fake.asked_names.borrow().len() <= 1);
    }

    #[test]
    fn a_launch_that_cannot_be_run_says_so() {
        let mut fake = game_pass();
        fake.spawn_fails = true;
        assert!(matches!(run(&fake).0, Outcome::Failed { .. }));
    }

    #[test]
    fn a_package_whose_manifests_are_unreadable_is_not_launched() {
        let mut fake = game_pass();
        fake.files.clear();
        assert!(matches!(
            run(&fake).0,
            Outcome::Planned(Plan::NotInstalled { .. })
        ));
    }

    #[test]
    fn reg_query_output_is_read() {
        let output = "\r\nHKEY_CURRENT_USER\\Software\\Valve\\Steam\r\n    SteamPath    REG_SZ    c:/program files (x86)/steam\r\n\r\n";
        assert_eq!(
            parse_reg_query(output, "SteamPath").as_deref(),
            Some("c:/program files (x86)/steam")
        );
        assert_eq!(parse_reg_query("ERROR: nothing", "SteamPath"), None);
        assert_eq!(
            parse_reg_query("    SteamPathX    REG_SZ    x", "SteamPath"),
            None
        );
    }

    #[test]
    fn get_appx_package_json_is_read() {
        let json = r#"[{"packageFamilyName":"PocketpairInc.Palworld_ad4psfrxyesvt","installLocation":"C:\\x","signatureKind":"Store","isFramework":false}]"#;
        let packages = parse_packages(json);
        assert_eq!(packages.len(), 1);
        assert!(packages[0].applications.is_empty());
        assert!(parse_packages("").is_empty());
        assert!(parse_packages("{}").is_empty());
    }

    /// Asks this machine. Ignored by default: it needs Windows and, to say
    /// anything, Palworld on Game Pass. Run with `--ignored` to see what a real
    /// survey finds; it starts nothing.
    #[test]
    #[ignore]
    fn survey_this_machine() {
        let machine = survey(&palworld(), &WindowsProbe);
        eprintln!("steam: {:#?}", machine.steam);
        eprintln!("xbox: {:#?}", machine.xbox);
        eprintln!("processes: {}", machine.processes.len());
        eprintln!("plan: {:#?}", client::plan(&palworld(), &machine, None));
    }
}
