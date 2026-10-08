//! `homerun-game client launch`: start a player's own copy of a game.
//! `homerun-game client plan`: say what Play would do, and start nothing.
//!
//! The desktop's Play button runs this, one invocation per press, separate
//! from the long-lived `supervise` runner a hosted server uses, so a launch
//! never touches a running server's session. It prints its plan as soon as
//! it has one (the UI opens the join modal on it while the game loads), then
//! one line saying whether the game appeared, and exits.
//!
//! Everything it decides is `homerun_core::engine::client`; everything it does
//! is `homerun_supervisor::client_launch`. This file is arguments and output.

use std::path::{Path, PathBuf};
use std::time::Duration;

use homerun_core::engine::client::{join, Launch, Plan};
use homerun_core::engine::GameDescriptor;
use homerun_supervisor::client_launch::{self, Outcome, WindowsProbe};
use serde::Serialize;
use serde_json::Value;

pub const HELP: &str = "homerun-game client launch <game.json or slug> [options]
homerun-game client plan <game.json or slug> [options]

launch starts your own copy of the game from Steam or Xbox Game Pass (Steam
first when both are installed), and waits for it to appear. plan says what
launch would do (which copy, or that none is installed, or that one is
already running) and starts nothing.

Options:
  --link-file <json>        The server's link as the API returned it, for a game
                            that joins by link (client.stores[].join \"url\")
  --timeout-seconds <n>     How long to wait for the game (default 120)
  --json                    One JSON line for the plan, one for the result";

/// What this command prints in `--json` mode, one line each.
#[derive(Debug, Serialize)]
#[serde(tag = "event")]
enum ClientEvent<'a> {
    /// The plan, before anything starts.
    #[serde(rename = "client-plan")]
    Plan { plan: &'a Plan },
    #[serde(rename = "client-started", rename_all = "camelCase")]
    Started { launch: &'a Launch, pid: u32 },
    #[serde(rename = "client-not-started", rename_all = "camelCase")]
    NotStarted { launch: &'a Launch },
    #[serde(rename = "client-failed", rename_all = "camelCase")]
    Failed {
        launch: &'a Launch,
        message: &'a str,
    },
}

impl ClientEvent<'_> {
    fn print(&self) {
        if let Ok(json) = serde_json::to_string(self) {
            println!("{json}");
        }
    }
}

pub fn run(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let only_plan = match args.next().as_deref() {
        Some("launch") => false,
        Some("plan") => true,
        Some("--help" | "-h") | None => {
            println!("{HELP}");
            return Ok(());
        }
        Some(other) => return Err(format!("Unknown client command: {other}\n\n{HELP}")),
    };

    let (mut descriptor, mut link_file, mut timeout, mut as_json) = (None, None, None, false);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => as_json = true,
            "--help" | "-h" => {
                println!("{HELP}");
                return Ok(());
            }
            "--link-file" | "--timeout-seconds" => {
                let value = args.next().ok_or_else(|| format!("{arg} needs a value."))?;
                let slot = if arg == "--link-file" {
                    &mut link_file
                } else {
                    &mut timeout
                };
                if slot.replace(value).is_some() {
                    return Err(format!("{arg} was supplied twice."));
                }
            }
            _ if !arg.starts_with('-') && descriptor.is_none() => descriptor = Some(arg),
            _ => return Err(format!("Unknown argument: {arg}")),
        }
    }

    let d = read_descriptor(&descriptor.ok_or("Choose a game descriptor or slug.")?)?;
    let address = match &link_file {
        Some(path) => {
            let link = read_json(path)?;
            join::address_from_link(&d.ports, &link)
        }
        None => None,
    };
    let timeout = match timeout {
        Some(s) => Duration::from_secs(
            s.parse::<u64>()
                .ok()
                .filter(|n| (1..=900).contains(n))
                .ok_or("--timeout-seconds is a whole number from 1 to 900.")?,
        ),
        None => client_launch::DEFAULT_TIMEOUT,
    };

    if only_plan {
        // What the desktop asks before anyone presses Play, to show the
        // button's state. One line, and nothing starts.
        let machine = client_launch::survey(&d, &WindowsProbe);
        let plan = homerun_core::engine::client::plan(&d, &machine, address.as_ref());
        if as_json {
            ClientEvent::Plan { plan: &plan }.print();
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(&plan).map_err(|e| e.to_string())?
            );
        }
        return Ok(());
    }

    let outcome = client_launch::launch(
        &d,
        &WindowsProbe,
        address.as_ref(),
        timeout,
        client_launch::POLL,
        &mut |plan| {
            if as_json {
                ClientEvent::Plan { plan }.print();
            }
        },
    );

    if as_json {
        match &outcome {
            Outcome::Planned(_) => {}
            Outcome::Started { launch, pid } => ClientEvent::Started { launch, pid: *pid }.print(),
            Outcome::NotStarted { launch } => ClientEvent::NotStarted { launch }.print(),
            Outcome::Failed { launch, message } => ClientEvent::Failed { launch, message }.print(),
        }
        // A refusal is an answer, reported on stdout; the exit code is for a
        // command that could not run at all.
        return Ok(());
    }
    human(&d, outcome)
}

fn human(d: &GameDescriptor, outcome: Outcome) -> Result<(), String> {
    let name = if d.name.is_empty() {
        d.id.as_str()
    } else {
        d.name.as_str()
    };
    match outcome {
        Outcome::Planned(Plan::AlreadyRunning { pid, install_dir, .. }) => Err(format!(
            "{name} is already running (process {pid}, from {install_dir}). If you can't see it, close it in Task Manager and try again."
        )),
        Outcome::Planned(Plan::NotInstalled { stores, .. }) => Err(format!(
            "{name} isn't installed from {}.",
            stores
                .iter()
                .map(|s| serde_json::to_value(s).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default())
                .collect::<Vec<_>>()
                .join(" or ")
        )),
        Outcome::Planned(Plan::NoStores) => Err(format!("Homerun can't start {name} for you.")),
        Outcome::Planned(Plan::Launch(_)) => unreachable!("a launch plan is always run"),
        Outcome::Started { launch, pid } => {
            println!("{name} started (process {pid}) from {}.", store_name(&launch));
            if let Some(reason) = &launch.join_refusal {
                println!("{reason}");
            }
            Ok(())
        }
        Outcome::NotStarted { launch } => Err(format!(
            "{name} didn't appear in time. {} may still be starting it.",
            store_name(&launch)
        )),
        Outcome::Failed { message, .. } => Err(message),
    }
}

fn store_name(launch: &Launch) -> &'static str {
    match launch.store {
        homerun_core::engine::descriptor::StoreKind::Steam => "Steam",
        homerun_core::engine::descriptor::StoreKind::Xbox => "Xbox",
        homerun_core::engine::descriptor::StoreKind::Unknown => "the store",
    }
}

fn read_descriptor(path: &str) -> Result<GameDescriptor, String> {
    let path = if Path::new(path).is_file() {
        PathBuf::from(path)
    } else {
        Path::new("games").join(path).join("game.json")
    };
    serde_json::from_value(read_json(&path.to_string_lossy())?)
        .map_err(|_| "This game's descriptor cannot be read.".to_string())
}

fn read_json(path: &str) -> Result<Value, String> {
    serde_json::from_slice(&std::fs::read(path).map_err(|_| format!("Cannot read {path}."))?)
        .map_err(|_| format!("{path} must contain valid JSON."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use homerun_core::engine::client::Program;
    use homerun_core::engine::descriptor::{JoinVia, StoreKind};

    fn launch() -> Launch {
        Launch {
            store: StoreKind::Xbox,
            program: Program::Explorer,
            args: vec![r"shell:AppsFolder\X.Y_0123456789abc!Game".into()],
            install_dir: r"C:\x".into(),
            executable: None,
            join: JoinVia::Info,
            join_refusal: None,
        }
    }

    /// The desktop reads these lines; the names are the contract.
    #[test]
    fn the_json_lines_are_spelled_for_the_desktop() {
        let plan = Plan::NotInstalled {
            stores: vec![StoreKind::Steam],
            pages: vec![homerun_core::engine::client::StorePage {
                store: StoreKind::Steam,
                url: "https://store.steampowered.com/app/105600/".into(),
            }],
        };
        let line = serde_json::to_value(ClientEvent::Plan { plan: &plan }).unwrap();
        assert_eq!(line["event"], "client-plan");
        assert_eq!(line["plan"]["outcome"], "not-installed");
        assert_eq!(line["plan"]["pages"][0]["store"], "steam");
        assert_eq!(line["plan"]["pages"][0]["url"], "https://store.steampowered.com/app/105600/");

        let l = launch();
        let started = serde_json::to_value(ClientEvent::Started { launch: &l, pid: 4 }).unwrap();
        assert_eq!(started["event"], "client-started");
        assert_eq!(started["pid"], 4);
        assert_eq!(started["launch"]["installDir"], r"C:\x");

        let failed = serde_json::to_value(ClientEvent::Failed {
            launch: &l,
            message: "m",
        })
        .unwrap();
        assert_eq!(failed["event"], "client-failed");
        let not = serde_json::to_value(ClientEvent::NotStarted { launch: &l }).unwrap();
        assert_eq!(not["event"], "client-not-started");
    }

    #[test]
    fn arguments_are_checked() {
        let run_with = |args: &[&str]| run(args.iter().map(|s| s.to_string()));
        assert!(run_with(&["start"]).is_err());
        assert!(run_with(&["plan"]).is_err(), "needs a game");
        assert!(run_with(&["launch"]).is_err(), "needs a game");
        assert!(run_with(&["launch", "x", "--timeout-seconds"]).is_err());
        assert!(run_with(&["launch", "x", "--link-file", "a", "--link-file", "b"]).is_err());
        assert!(run_with(&["launch", "x", "--bogus"]).is_err());
        assert!(run_with(&["--help"]).is_ok());
    }
}
