//! Starting a player's own copy of a game: the decisions half.
//!
//! # What this is
//!
//! The Play button on a server for a descriptor game. The player owns the
//! game on Steam or on Xbox Game Pass; this decides which copy to start,
//! exactly what to run, and whether that also joins the server or the player
//! is shown the address. It reads `client.stores` from the descriptor.
//!
//! There is no store-neutral way to start a game. Every launcher that covers
//! several stores (Playnite, GOG Galaxy, Lutris) keeps one small adapter per
//! store, reading that store's own record, and this is the same shape:
//! [`steam`] and [`xbox`].
//!
//! # The split
//!
//! Pure, as the rest of the crate is. The runner (`homerun-game client
//! launch`) reads the registry and the files, asks Windows for its packages
//! and processes, hands the results here, and runs what comes back. Nothing
//! in this module opens a file or starts a process.
//!
//! # Rules that keep it safe
//!
//!  - **The program is one of two, fixed here.** Steam's `steam.exe`, from a
//!    path held to that shape ([`steam::steam_exe`]), or Windows'
//!    `explorer.exe`. A launch is a program and an argument list; no string
//!    is ever handed to a shell to interpret.
//!  - **Every id is checked before it is used,** here, not trusted because
//!    validation passed somewhere else: a Steam app id is a number, a family
//!    name and an application id are held to [`xbox`]'s shapes.
//!  - **A join link's scheme is decided by [`join`],** from a one-entry
//!    allowlist, before anything is substituted into it.
//!
//! # Steam first
//!
//! When both copies are installed, Steam's starts. That is a product
//! decision, not a technical one, and [`PREFERENCE`] is where it lives.
//!
//! # Already running
//!
//! Palworld can leave its process running after the player quits, with no
//! window, until it is killed in Task Manager; while it is there, a launch
//! starts nothing. So a process under the install folder is not proof that a
//! launch worked. [`plan`] refuses with [`Plan::AlreadyRunning`] when one is
//! there before the launch, and [`Launch::started`] counts only a process
//! that was not there before: the game's own executable when the package
//! names it ([`xbox::Package::executable_for`]), any new process in the
//! install folder when it does not.

pub mod join;
#[cfg(feature = "client-manifests")]
pub mod manifest;
pub mod steam;
pub mod xbox;

use serde::{Deserialize, Serialize};

use super::descriptor::{ClientStore, GameDescriptor, JoinVia, StoreKind};
use join::JoinAddress;

/// Which installed copy starts, in order.
pub const PREFERENCE: [StoreKind; 2] = [StoreKind::Steam, StoreKind::Xbox];

/// A running process, as the runner listed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Process {
    pub pid: u32,
    /// The executable's full path. A process whose path the runner could not
    /// read is left out of the list, not listed with an empty one.
    pub path: String,
}

/// What the runner found on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Machine {
    /// `None` when Steam is not installed.
    #[serde(default)]
    pub steam: Option<steam::SteamFound>,
    /// The Store packages the runner asked Windows about: the families the
    /// descriptor declares.
    #[serde(default)]
    pub xbox: Vec<xbox::Package>,
    #[serde(default)]
    pub processes: Vec<Process>,
}

/// The program a launch runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Program {
    /// This path, checked by [`steam::steam_exe`].
    Steam { exe: String },
    /// `%SystemRoot%\explorer.exe`. The runner resolves the directory; the
    /// name is not negotiable.
    Explorer,
}

/// A launch, ready to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Launch {
    pub store: StoreKind,
    pub program: Program,
    pub args: Vec<String>,
    /// Where the game's processes run from, for [`Launch::started`].
    pub install_dir: String,
    /// The game's own executable, when the store says which it is. Only Game
    /// Pass does, through `MicrosoftGame.config`; see [`xbox`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    /// What this launch does about the server: [`JoinVia::Url`] only when the
    /// link was built and is in `args`.
    pub join: JoinVia,
    /// Why a store that joins by link is only starting the game this time,
    /// in a sentence for the player. The address is shown instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join_refusal: Option<String>,
}

/// What Play does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum Plan {
    Launch(Launch),
    /// A copy is already running from this install. Nothing is started.
    #[serde(rename_all = "camelCase")]
    AlreadyRunning {
        store: StoreKind,
        pid: u32,
        install_dir: String,
    },
    /// The game declares stores and none has it installed.
    NotInstalled {
        stores: Vec<StoreKind>,
        /// Where to get it, in [`PREFERENCE`] order: each store's page for
        /// this game, for the stores the descriptor gives one for.
        pages: Vec<StorePage>,
    },
    /// The descriptor declares no store this build can start.
    NoStores,
}

/// Decide what Play does on this machine.
///
/// `address` is the server's join address, when the caller has one; it is
/// needed only by a store that joins by link.
pub fn plan(descriptor: &GameDescriptor, machine: &Machine, address: Option<&JoinAddress>) -> Plan {
    let declared: Vec<StoreKind> = PREFERENCE
        .into_iter()
        .filter(|kind| entry(descriptor, *kind).is_some())
        .collect();
    if declared.is_empty() {
        return Plan::NoStores;
    }

    let candidates: Vec<Launch> = PREFERENCE
        .into_iter()
        .filter_map(|kind| {
            let store = entry(descriptor, kind)?;
            match kind {
                StoreKind::Steam => steam_candidate(descriptor, store, machine, address),
                StoreKind::Xbox => xbox_candidate(store, machine),
                StoreKind::Unknown => None,
            }
        })
        .collect();

    // Any installed copy already running, not only the preferred one: a
    // second copy of a game from another store is not what anybody wants.
    for c in &candidates {
        if let Some(p) = machine
            .processes
            .iter()
            .find(|p| is_under(&c.install_dir, &p.path))
        {
            return Plan::AlreadyRunning {
                store: c.store,
                pid: p.pid,
                install_dir: c.install_dir.clone(),
            };
        }
    }

    match candidates.into_iter().next() {
        Some(launch) => Plan::Launch(launch),
        None => Plan::NotInstalled {
            stores: declared,
            pages: store_pages(descriptor),
        },
    }
}

impl Launch {
    /// The new process this launch started, if one has appeared.
    ///
    /// The game's own executable when it is known: `GameLaunchHelper.exe`
    /// runs from the install folder too, and counting it would call a launch
    /// good whose game then failed to start. Otherwise [`started`].
    pub fn started(&self, before: &[Process], now: &[Process]) -> Option<u32> {
        match &self.executable {
            Some(exe) => {
                let exe = normal(exe);
                now.iter()
                    .filter(|p| normal(&p.path) == exe)
                    .find(|p| !before.iter().any(|b| b.pid == p.pid))
                    .map(|p| p.pid)
            }
            None => started(&self.install_dir, before, now),
        }
    }
}

/// The new process a launch started, if one has appeared.
///
/// `before` is the process list taken just before the launch. A process under
/// the install folder that was in it does not count, which is what keeps a
/// lingering copy from reading as a successful launch.
pub fn started(install_dir: &str, before: &[Process], now: &[Process]) -> Option<u32> {
    now.iter()
        .filter(|p| is_under(install_dir, &p.path))
        .find(|p| !before.iter().any(|b| b.pid == p.pid))
        .map(|p| p.pid)
}

/// A store's page for a game, where a player who does not have it can get it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorePage {
    pub store: StoreKind,
    pub url: String,
}

/// Each declared store's page for the game, in [`PREFERENCE`] order.
///
/// Built here from checked ids, on two fixed hosts, so whatever opens the
/// link opens a store and nothing else: Steam's from `appId`, the Microsoft
/// Store's from `storeId` (a Game Pass entry without one has no page).
pub fn store_pages(d: &GameDescriptor) -> Vec<StorePage> {
    PREFERENCE
        .into_iter()
        .filter_map(|kind| {
            let entry = entry(d, kind)?;
            let url = match kind {
                StoreKind::Steam => {
                    format!("https://store.steampowered.com/app/{}/", entry.app_id?)
                }
                StoreKind::Xbox => {
                    let id = entry
                        .store_id
                        .as_deref()
                        .filter(|id| xbox::is_store_id(id))?;
                    format!("https://apps.microsoft.com/detail/{id}")
                }
                StoreKind::Unknown => return None,
            };
            Some(StorePage { store: kind, url })
        })
        .collect()
}

/// The first entry for a store with the ids that store needs, checked.
fn entry(d: &GameDescriptor, kind: StoreKind) -> Option<&ClientStore> {
    d.client.stores.iter().find(|s| {
        s.store == kind
            && match kind {
                StoreKind::Steam => s.app_id.is_some_and(|id| id != 0),
                StoreKind::Xbox => {
                    s.package_family_name
                        .as_deref()
                        .is_some_and(xbox::is_package_family_name)
                        && s.application_id
                            .as_deref()
                            .is_some_and(xbox::is_application_id)
                }
                StoreKind::Unknown => false,
            }
    })
}

fn steam_candidate(
    d: &GameDescriptor,
    store: &ClientStore,
    machine: &Machine,
    address: Option<&JoinAddress>,
) -> Option<Launch> {
    let app_id = store.app_id?;
    let found = machine.steam.as_ref()?;
    let exe = steam::steam_exe(&found.exe)?;
    let installed = found
        .installed
        .iter()
        .find(|i| i.manifest.app_id == app_id)?;

    // `-silent` keeps Steam's own window from opening over the game.
    let mut join = JoinVia::Info;
    let mut join_refusal = None;
    let mut args = vec!["-silent".to_string(), format!("steam://rungameid/{app_id}")];
    match store.join {
        JoinVia::Url => {
            let built = match (d.client.join_url.as_deref(), address) {
                (Some(template), Some(address)) => join::build_join_url(template.trim(), address),
                (None, _) => Err(join::NO_LINK),
                (Some(_), None) => Err(join::NO_PUBLIC_PORT),
            };
            match built {
                Ok(url) => {
                    args = vec!["-silent".into(), url];
                    join = JoinVia::Url;
                }
                Err(reason) => join_refusal = Some(reason.to_string()),
            }
        }
        JoinVia::Args => {
            let built = match (d.client.join_args.as_deref(), address) {
                (Some(template), Some(address)) => join::build_join_args(template, address),
                (None, _) => Err(join::NO_LINK),
                (Some(_), None) => Err(join::NO_PUBLIC_PORT),
            };
            match built {
                // The program and everything before the game's own
                // arguments are fixed here; only the template's checked
                // elements follow `-applaunch <appId>`.
                Ok(game_args) => {
                    args = vec!["-silent".into(), "-applaunch".into(), app_id.to_string()];
                    args.extend(game_args);
                    join = JoinVia::Args;
                }
                Err(reason) => join_refusal = Some(reason.to_string()),
            }
        }
        JoinVia::Info => {}
    }

    Some(Launch {
        store: StoreKind::Steam,
        install_dir: installed.install_dir(),
        // Steam records no executable per app, so the folder is watched.
        executable: None,
        program: Program::Steam { exe },
        args,
        join,
        join_refusal,
    })
}

fn xbox_candidate(store: &ClientStore, machine: &Machine) -> Option<Launch> {
    let family = store.package_family_name.as_deref()?;
    let application = store.application_id.as_deref()?;
    let package = xbox::find(&machine.xbox, family, application).ok()?;
    let target = xbox::apps_folder_target(family, application)?;
    Some(Launch {
        store: StoreKind::Xbox,
        install_dir: package.install_location.clone(),
        executable: package.executable_for(application),
        program: Program::Explorer,
        args: vec![target],
        // Xbox has no connect link; validation refuses `url` for it.
        join: JoinVia::Info,
        join_refusal: None,
    })
}

/// Whether `path` is inside `dir`, as Windows compares paths: either slash,
/// any case, with or without the `\\?\` prefix. `D:\Games\Palworld2` is not
/// inside `D:\Games\Palworld`.
pub fn is_under(dir: &str, path: &str) -> bool {
    let dir = normal(dir);
    let path = normal(path);
    !dir.is_empty()
        && path.len() > dir.len() + 1
        && path.starts_with(&dir)
        && path.as_bytes()[dir.len()] == b'\\'
}

fn normal(path: &str) -> String {
    let path = path.replace('/', "\\");
    let path = path.strip_prefix(r"\\?\").unwrap_or(&path);
    path.trim_end_matches('\\').to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::descriptor::Client;
    use std::collections::BTreeMap;

    const PALWORLD_FAMILY: &str = "PocketpairInc.Palworld_ad4psfrxyesvt";
    const XBOX_DIR: &str =
        r"C:\Program Files\WindowsApps\PocketpairInc.Palworld_1.10.2999.0_x64__ad4psfrxyesvt";
    const XBOX_GAME: &str = r"C:\Program Files\WindowsApps\PocketpairInc.Palworld_1.10.2999.0_x64__ad4psfrxyesvt\Pal\Binaries\WinGDK\Palworld-WinGDK-Shipping.exe";
    const STEAM_DIR: &str = r"D:\SteamLibrary\steamapps\common\Palworld";
    const STEAM_GAME: &str =
        r"D:\SteamLibrary\steamapps\common\Palworld\Pal\Binaries\Win64\Palworld-Win64-Shipping.exe";

    fn palworld(steam_join: JoinVia) -> GameDescriptor {
        GameDescriptor {
            id: "palworld".into(),
            client: Client {
                join_url: Some("steam://connect/{host}:{port:game}".into()),
                stores: vec![
                    // Xbox first on purpose: the order in the file is not the preference.
                    ClientStore {
                        store: StoreKind::Xbox,
                        package_family_name: Some(PALWORLD_FAMILY.into()),
                        application_id: Some("AppPalShipping".into()),
                        ..Default::default()
                    },
                    ClientStore {
                        store: StoreKind::Steam,
                        app_id: Some(1623730),
                        join: steam_join,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn steam_found() -> steam::SteamFound {
        steam::SteamFound {
            exe: "c:/program files (x86)/steam/steam.exe".into(),
            installed: vec![steam::Installed {
                library: r"D:\SteamLibrary".into(),
                manifest: steam::AppManifest {
                    app_id: 1623730,
                    install_dir: "Palworld".into(),
                    state_flags: 4,
                },
            }],
        }
    }

    fn xbox_found() -> Vec<xbox::Package> {
        vec![xbox::Package {
            package_family_name: PALWORLD_FAMILY.into(),
            install_location: XBOX_DIR.into(),
            signature_kind: "Store".into(),
            is_framework: false,
            applications: vec!["AppPalShipping".into()],
            executables: vec![xbox::GameExecutable {
                id: "AppPalShipping".into(),
                name: r"Pal\Binaries\WinGDK\Palworld-WinGDK-Shipping.exe".into(),
            }],
        }]
    }

    fn address() -> JoinAddress {
        JoinAddress {
            host: "us-east.gethomerun.app".into(),
            ports: BTreeMap::from([("game".into(), 20011)]),
        }
    }

    fn process(pid: u32, path: &str) -> Process {
        Process {
            pid,
            path: path.into(),
        }
    }

    #[test]
    fn game_pass_alone_starts_through_explorer() {
        let machine = Machine {
            xbox: xbox_found(),
            ..Default::default()
        };
        let Plan::Launch(launch) = plan(&palworld(JoinVia::Info), &machine, None) else {
            panic!()
        };
        assert_eq!(launch.store, StoreKind::Xbox);
        assert_eq!(launch.program, Program::Explorer);
        assert_eq!(
            launch.args,
            [r"shell:AppsFolder\PocketpairInc.Palworld_ad4psfrxyesvt!AppPalShipping"]
        );
        assert_eq!(launch.install_dir, XBOX_DIR);
        assert_eq!(launch.join, JoinVia::Info);
    }

    #[test]
    fn steam_wins_when_both_are_installed() {
        let machine = Machine {
            steam: Some(steam_found()),
            xbox: xbox_found(),
            processes: vec![],
        };
        let Plan::Launch(launch) = plan(&palworld(JoinVia::Info), &machine, None) else {
            panic!()
        };
        assert_eq!(launch.store, StoreKind::Steam);
        assert_eq!(
            launch.program,
            Program::Steam {
                exe: r"c:\program files (x86)\steam\steam.exe".into()
            }
        );
        assert_eq!(launch.args, ["-silent", "steam://rungameid/1623730"]);
        assert_eq!(launch.install_dir, STEAM_DIR);
    }

    #[test]
    fn steam_installed_without_this_game_falls_through_to_game_pass() {
        let mut found = steam_found();
        found.installed.clear();
        let machine = Machine {
            steam: Some(found),
            xbox: xbox_found(),
            processes: vec![],
        };
        let Plan::Launch(launch) = plan(&palworld(JoinVia::Info), &machine, None) else {
            panic!()
        };
        assert_eq!(launch.store, StoreKind::Xbox);
    }

    #[test]
    fn a_game_that_honours_connect_starts_and_joins() {
        let machine = Machine {
            steam: Some(steam_found()),
            ..Default::default()
        };
        let Plan::Launch(launch) = plan(&palworld(JoinVia::Url), &machine, Some(&address())) else {
            panic!()
        };
        assert_eq!(
            launch.args,
            ["-silent", "steam://connect/us-east.gethomerun.app:20011"]
        );
        assert_eq!(launch.join, JoinVia::Url);
        assert_eq!(launch.join_refusal, None);
    }

    #[test]
    fn a_connect_link_that_cannot_be_built_still_starts_the_game() {
        let machine = Machine {
            steam: Some(steam_found()),
            ..Default::default()
        };
        let Plan::Launch(launch) = plan(&palworld(JoinVia::Url), &machine, None) else {
            panic!()
        };
        assert_eq!(launch.args, ["-silent", "steam://rungameid/1623730"]);
        assert_eq!(launch.join, JoinVia::Info);
        assert_eq!(launch.join_refusal.as_deref(), Some(join::NO_PUBLIC_PORT));
    }

    /// Terraria's shape: Steam starts the game with its join arguments.
    #[test]
    fn a_game_that_takes_join_arguments_is_started_with_them() {
        let mut d = palworld(JoinVia::Args);
        d.client.join_args = Some(
            ["-join", "{host}", "-port", "{port:game}"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        let machine = Machine {
            steam: Some(steam_found()),
            ..Default::default()
        };
        let Plan::Launch(launch) = plan(&d, &machine, Some(&address())) else {
            panic!()
        };
        assert_eq!(
            launch.args,
            [
                "-silent",
                "-applaunch",
                "1623730",
                "-join",
                "us-east.gethomerun.app",
                "-port",
                "20011"
            ]
        );
        assert_eq!(launch.join, JoinVia::Args);
        assert_eq!(launch.join_refusal, None);

        // No address: the game still starts, and the player is told why it
        // won't join by itself.
        let Plan::Launch(launch) = plan(&d, &machine, None) else {
            panic!()
        };
        assert_eq!(launch.args, ["-silent", "steam://rungameid/1623730"]);
        assert_eq!(launch.join, JoinVia::Info);
        assert_eq!(launch.join_refusal.as_deref(), Some(join::NO_PUBLIC_PORT));
    }

    #[test]
    fn nothing_installed_names_the_stores_and_their_pages() {
        let mut d = palworld(JoinVia::Info);
        // Without a store id, Game Pass has no page to link to.
        assert_eq!(
            plan(&d, &Machine::default(), None),
            Plan::NotInstalled {
                stores: vec![StoreKind::Steam, StoreKind::Xbox],
                pages: vec![StorePage {
                    store: StoreKind::Steam,
                    url: "https://store.steampowered.com/app/1623730/".into()
                }],
            }
        );

        d.client.stores[0].store_id = Some("9NKV34XDW014".into());
        let Plan::NotInstalled { pages, .. } = plan(&d, &Machine::default(), None) else {
            panic!()
        };
        assert_eq!(
            pages.iter().map(|p| p.url.as_str()).collect::<Vec<_>>(),
            [
                "https://store.steampowered.com/app/1623730/",
                "https://apps.microsoft.com/detail/9NKV34XDW014"
            ]
        );

        // A store id out of shape is no page, not a link to somewhere else.
        d.client.stores[0].store_id = Some("../../evil".into());
        let Plan::NotInstalled { pages, .. } = plan(&d, &Machine::default(), None) else {
            panic!()
        };
        assert_eq!(pages.len(), 1);
    }

    #[test]
    fn a_game_with_no_usable_store_says_so() {
        let mut d = palworld(JoinVia::Info);
        d.client.stores = vec![
            ClientStore {
                store: StoreKind::Unknown,
                ..Default::default()
            },
            ClientStore {
                store: StoreKind::Steam,
                app_id: None,
                ..Default::default()
            },
            ClientStore {
                store: StoreKind::Xbox,
                package_family_name: Some("not a family".into()),
                application_id: Some("Game".into()),
                ..Default::default()
            },
        ];
        assert_eq!(
            plan(
                &d,
                &Machine {
                    steam: Some(steam_found()),
                    xbox: xbox_found(),
                    processes: vec![]
                },
                None
            ),
            Plan::NoStores
        );
    }

    #[test]
    fn a_steam_path_that_is_not_steam_exe_is_not_run() {
        let mut found = steam_found();
        found.exe = r"C:\Users\x\Downloads\steam.exe.bat".into();
        let machine = Machine {
            steam: Some(found),
            ..Default::default()
        };
        assert!(matches!(
            plan(&palworld(JoinVia::Info), &machine, None),
            Plan::NotInstalled { .. }
        ));
    }

    /// The case that was seen: Palworld quit, its process stayed, and the
    /// next launch started nothing.
    #[test]
    fn a_lingering_copy_is_already_running_not_a_launch() {
        let machine = Machine {
            xbox: xbox_found(),
            processes: vec![process(180936, XBOX_GAME)],
            ..Default::default()
        };
        assert_eq!(
            plan(&palworld(JoinVia::Info), &machine, None),
            Plan::AlreadyRunning {
                store: StoreKind::Xbox,
                pid: 180936,
                install_dir: XBOX_DIR.into()
            }
        );
    }

    #[test]
    fn a_copy_running_from_the_other_store_also_counts() {
        let machine = Machine {
            steam: Some(steam_found()),
            xbox: xbox_found(),
            processes: vec![process(7, XBOX_GAME)],
        };
        assert!(matches!(
            plan(&palworld(JoinVia::Info), &machine, None),
            Plan::AlreadyRunning {
                store: StoreKind::Xbox,
                ..
            }
        ));
    }

    /// The launch stub runs from the install folder too. When the package
    /// names the game behind it, only that counts.
    #[test]
    fn game_pass_waits_for_the_game_not_the_launch_stub() {
        let machine = Machine {
            xbox: xbox_found(),
            ..Default::default()
        };
        let Plan::Launch(launch) = plan(&palworld(JoinVia::Info), &machine, None) else {
            panic!()
        };
        assert_eq!(launch.executable.as_deref(), Some(XBOX_GAME));

        let stub = process(500, &format!(r"{XBOX_DIR}\GameLaunchHelper.exe"));
        assert_eq!(launch.started(&[], std::slice::from_ref(&stub)), None);
        let game = process(501, &XBOX_GAME.to_uppercase());
        assert_eq!(launch.started(&[], &[stub, game.clone()]), Some(501));
        let before = [game.clone()];
        assert_eq!(
            launch.started(&before, &[game]),
            None,
            "not if it was there before"
        );
    }

    #[test]
    fn without_the_games_executable_any_new_process_in_the_folder_counts() {
        let mut packages = xbox_found();
        packages[0].executables.clear();
        let machine = Machine {
            xbox: packages,
            ..Default::default()
        };
        let Plan::Launch(launch) = plan(&palworld(JoinVia::Info), &machine, None) else {
            panic!()
        };
        assert_eq!(launch.executable, None);
        let stub = process(500, &format!(r"{XBOX_DIR}\GameLaunchHelper.exe"));
        assert_eq!(launch.started(&[], &[stub]), Some(500));

        let machine = Machine {
            steam: Some(steam_found()),
            ..Default::default()
        };
        let Plan::Launch(launch) = plan(&palworld(JoinVia::Info), &machine, None) else {
            panic!()
        };
        assert_eq!(
            launch.executable, None,
            "Steam records no executable per app"
        );
        assert_eq!(launch.started(&[], &[process(9, STEAM_GAME)]), Some(9));
    }

    #[test]
    fn only_a_process_that_was_not_there_before_counts_as_started() {
        let lingering = process(180936, XBOX_GAME);
        let fresh = process(175896, XBOX_GAME);
        let unrelated = process(1, r"C:\Windows\explorer.exe");
        let before = [lingering.clone()];

        assert_eq!(
            started(XBOX_DIR, &before, &[lingering.clone(), unrelated.clone()]),
            None
        );
        assert_eq!(
            started(XBOX_DIR, &before, &[lingering, fresh, unrelated]),
            Some(175896)
        );
        assert_eq!(started(STEAM_DIR, &[], &[process(9, STEAM_GAME)]), Some(9));
    }

    #[test]
    fn paths_compare_the_way_windows_compares_them() {
        assert!(is_under(STEAM_DIR, STEAM_GAME));
        assert!(is_under(&STEAM_DIR.to_uppercase(), STEAM_GAME));
        assert!(is_under(&format!("{STEAM_DIR}\\"), STEAM_GAME));
        assert!(is_under(STEAM_DIR, &STEAM_GAME.replace('\\', "/")));
        assert!(is_under(STEAM_DIR, &format!(r"\\?\{STEAM_GAME}")));

        assert!(!is_under(
            STEAM_DIR,
            r"D:\SteamLibrary\steamapps\common\Palworld2\x.exe"
        ));
        assert!(!is_under(STEAM_DIR, STEAM_DIR));
        assert!(!is_under("", STEAM_GAME));
    }

    #[test]
    fn the_plan_is_spelled_for_the_runner() {
        let machine = Machine {
            xbox: xbox_found(),
            ..Default::default()
        };
        let json = serde_json::to_value(plan(&palworld(JoinVia::Info), &machine, None)).unwrap();
        assert_eq!(json["outcome"], "launch");
        assert_eq!(json["store"], "xbox");
        assert_eq!(json["program"]["kind"], "explorer");
        assert_eq!(json["installDir"], XBOX_DIR);
        assert_eq!(json["join"], "info");
        assert!(json.get("joinRefusal").is_none());

        let running = serde_json::to_value(Plan::AlreadyRunning {
            store: StoreKind::Steam,
            pid: 3,
            install_dir: "x".into(),
        })
        .unwrap();
        assert_eq!(running["outcome"], "already-running");
        assert_eq!(running["installDir"], "x");

        let machine: Machine = serde_json::from_str(
            r#"{ "xbox": [{ "packageFamilyName": "PocketpairInc.Palworld_ad4psfrxyesvt",
                 "installLocation": "C:\\x", "signatureKind": "Store", "isFramework": false,
                 "applications": ["AppPalShipping"] }],
                 "processes": [{ "pid": 4, "path": "C:\\y.exe" }] }"#,
        )
        .unwrap();
        assert_eq!(machine.xbox.len(), 1);
        assert!(machine.steam.is_none());
    }
}
