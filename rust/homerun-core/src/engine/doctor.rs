//! Can this machine host this game, and what will be wrong if it tries?
//!
//! # Refusals and warnings are different answers
//!
//! A **problem** means do not start: the machine cannot run this game, or the
//! descriptor does not describe one. A **warning** means it will start and
//! something will be worse than the player expects — not enough memory for
//! the size of world they asked for, a port group Homerun cannot publish yet.
//!
//! Keeping them apart is what lets the same verdict drive three callers that
//! want different strictness: `game doctor` prints both, create-time gating
//! blocks on problems only, and the onboarding pipeline turns warnings into
//! the YELLOW half of a dossier — a named diff against `games/PLATFORM.md`
//! rather than a bug.
//!
//! # Heavy games are in scope
//!
//! A game that only runs on a strong PC is acceptable. `requires` is what
//! gates it here and at create time, rather than the pipeline avoiding games
//! people actually want to host.

use serde::{Deserialize, Serialize};

use super::descriptor::GameDescriptor;
use super::{fetch, licence, ports, validate};

/// What the host knows about the machine it is running on.
///
/// Every field is supplied rather than measured: this crate has no way to ask
/// an operating system anything, which is what keeps its suite device-free.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Machine {
    /// `win32-x64`, and today that is the only value that yields a verdict.
    pub host: String,
    pub ram_mb: u64,
    /// Free space where runtimes and servers live, not total.
    pub disk_mb: u64,
    #[serde(default)]
    pub cpu_cores: Option<u32>,
}

/// The answer.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Verdict {
    /// True when nothing in `problems` stands in the way. Warnings do not
    /// clear it and do not set it.
    pub ok: bool,
    /// Reasons not to start, each a sentence a player can read.
    pub problems: Vec<String>,
    /// Things that will be worse than expected.
    pub warnings: Vec<String>,
    /// Whether the runtime is already on this machine — the difference
    /// between a ten-second first launch and a nine-gigabyte one.
    pub runtime_present: bool,
    /// The licence refusal, when that is one of the reasons not to start.
    ///
    /// The same sentence also appears in `problems`, which is deliberate: a
    /// caller that only reads `problems` keeps working exactly as it did.
    /// What this adds is the *kind* of a problem, which a list of sentences
    /// cannot carry — and the protocol has a code for this one
    /// (`licence_not_accepted`) that a caller could not previously reach,
    /// because every doctor problem went out as `requires_unmet`. Nobody can
    /// act on "this computer is not ready" when the answer is "somebody has
    /// to accept the terms".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub licence: Option<String>,
}

/// Judge a machine against a game.
///
/// `present` is what the host found in the runtime directory, and
/// `licence_accepted` is its record of a person's decision — never inferred
/// here, see [`super::licence`].
pub fn doctor(
    descriptor: &GameDescriptor,
    machine: &Machine,
    present: &fetch::Present,
    licence_accepted: bool,
) -> Verdict {
    let mut problems = Vec::new();
    let mut warnings = Vec::new();

    // A descriptor that does not describe a game is the first thing to say,
    // because every check below would otherwise report its consequences
    // instead of its cause.
    let report = validate::report(descriptor);
    for fault in &report.problems {
        problems.push(fault.clone());
    }
    warnings.extend(report.warnings.iter().cloned());

    if !descriptor.runs_on(&machine.host) {
        problems.push(format!(
            "{} cannot be hosted on this computer. It runs on {}.",
            display_name(descriptor),
            human_list(&descriptor.hosts)
        ));
    }

    let requires = &descriptor.requires;
    if requires.ram_mb > 0 && machine.ram_mb > 0 && machine.ram_mb < requires.ram_mb {
        problems.push(format!(
            "{} needs {} of memory and this computer has {}.",
            display_name(descriptor),
            gigabytes(requires.ram_mb),
            gigabytes(machine.ram_mb)
        ));
    }

    // Disk is checked against what still has to be downloaded. A machine with
    // the runtime already on it does not need room for it a second time.
    let runtime_present = matches!(
        fetch::plan(descriptor, &machine.host, "", present),
        Ok(fetch::Plan::AlreadyPresent { .. })
    );
    let needed = if runtime_present { 0 } else { requires.disk_mb };
    if needed > 0 && machine.disk_mb > 0 && machine.disk_mb < needed {
        problems.push(format!(
            "{} needs {} of free space and this drive has {}.",
            display_name(descriptor),
            gigabytes(needed),
            gigabytes(machine.disk_mb)
        ));
    }

    if let (Some(wanted), Some(have)) = (requires.cpu_cores, machine.cpu_cores) {
        if have < wanted {
            warnings.push(format!(
                "{} expects {wanted} processor cores and this computer has {have}. \
                 It will run, and it may struggle.",
                display_name(descriptor)
            ));
        }
    }

    let mut licence_problem = None;
    if licence::gate(descriptor, licence_accepted).is_err() {
        if let Some(terms) = licence::terms(descriptor) {
            let sentence = format!(
                "Nobody has accepted {} yet. {} cannot be downloaded until somebody does.",
                terms.name,
                display_name(descriptor)
            );
            problems.push(sentence.clone());
            licence_problem = Some(sentence);
        }
    }

    if let Some(why) = ports::fits_one_service(&descriptor.ports) {
        warnings.push(format!(
            "{} Players will be able to reach only the first group.",
            capitalise(&why)
        ));
    }

    Verdict {
        ok: problems.is_empty(),
        problems,
        warnings,
        runtime_present,
        licence: licence_problem,
    }
}

fn display_name(descriptor: &GameDescriptor) -> String {
    if descriptor.name.is_empty() {
        "This game".to_string()
    } else {
        descriptor.name.clone()
    }
}

/// Megabytes as something a person reads, rounded the way a person would.
fn gigabytes(mb: u64) -> String {
    if mb < 1024 {
        return format!("{mb} MB");
    }
    let gb = mb as f64 / 1024.0;
    if (gb - gb.round()).abs() < 0.05 {
        format!("{} GB", gb.round() as u64)
    } else {
        format!("{gb:.1} GB")
    }
}

fn human_list(items: &[String]) -> String {
    match items.len() {
        0 => "no computer Homerun supports".to_string(),
        1 => items[0].clone(),
        _ => {
            let (last, rest) = items.split_last().unwrap();
            format!("{} and {last}", rest.join(", "))
        }
    }
}

fn capitalise(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rust() -> GameDescriptor {
        GameDescriptor::parse(include_str!("testdata/rust.json")).unwrap()
    }

    fn capable() -> Machine {
        Machine {
            host: "win32-x64".into(),
            ram_mb: 32768,
            disk_mb: 400_000,
            cpu_cores: Some(8),
        }
    }

    const NOTHING: fetch::Present = fetch::Present {
        build_id: None,
        suspect: false,
    };

    #[test]
    fn a_capable_machine_with_accepted_terms_is_cleared() {
        let v = doctor(&rust(), &capable(), &NOTHING, true);
        assert!(v.ok, "{:?}", v.problems);
        assert!(v.problems.is_empty());
        assert!(!v.runtime_present);
    }

    #[test]
    fn too_little_memory_is_a_refusal_in_units_a_person_reads() {
        let machine = Machine {
            ram_mb: 4096,
            ..capable()
        };
        let v = doctor(&rust(), &machine, &NOTHING, true);
        assert!(!v.ok);
        let why = v.problems.join(" ");
        assert!(why.contains("8 GB") && why.contains("4 GB"), "{why}");
    }

    #[test]
    fn a_machine_this_game_does_not_run_on_is_told_what_it_does_run_on() {
        let machine = Machine {
            host: "linux-x64".into(),
            ..capable()
        };
        let v = doctor(&rust(), &machine, &NOTHING, true);
        assert!(!v.ok);
        assert!(
            v.problems.iter().any(|p| p.contains("win32-x64")),
            "{:?}",
            v.problems
        );
    }

    /// The gate that has to hold everywhere, including here.
    #[test]
    fn terms_nobody_has_accepted_are_a_refusal() {
        let v = doctor(&rust(), &capable(), &NOTHING, false);
        assert!(!v.ok);
        assert!(
            v.problems.iter().any(|p| p.contains("Facepunch")),
            "{:?}",
            v.problems
        );
    }

    /// "Nobody has accepted the terms" is not a fact about this computer, and
    /// the protocol has a code of its own for it. A list of sentences cannot
    /// carry the *kind* of a problem, so the licence refusal is named as well
    /// as listed — and it stays in `problems` too, so a caller that only
    /// reads that list is unaffected.
    #[test]
    fn the_licence_refusal_is_named_as_well_as_listed() {
        let v = doctor(&rust(), &capable(), &NOTHING, false);
        let named = v.licence.as_deref().expect("the licence is the blocker");
        assert!(named.contains("Facepunch"), "{named}");
        assert!(
            v.problems.iter().any(|p| p == named),
            "a caller reading only `problems` must still see it: {:?}",
            v.problems
        );
    }

    /// And it is absent when it is not the reason, so a caller cannot route
    /// an unrelated refusal to a licence prompt.
    #[test]
    fn nothing_is_named_when_the_terms_are_accepted() {
        assert!(doctor(&rust(), &capable(), &NOTHING, true)
            .licence
            .is_none());

        // A game with no terms of its own never has this problem, whatever
        // the host's record says. `licence: null` is not "accepted".
        let no_terms: GameDescriptor = serde_json::from_value(json!({
            "id": "g", "name": "G", "hosts": ["win32-x64"],
            "ready": { "marker": "up", "timeoutMs": 1000 },
            "console": { "via": "stdin" },
            "stop": { "via": "console", "command": "quit", "graceMs": 1000 },
            "platforms": { "win32-x64": {
                "runtime": { "source": "direct", "url": "https://x/y.zip", "sha256": "a".repeat(64) },
                "launch": { "exe": "S.exe" } } }
        }))
        .unwrap();
        assert!(doctor(&no_terms, &capable(), &NOTHING, false)
            .licence
            .is_none());
    }

    /// Disk is what is still to download. A machine that already has the
    /// runtime is not asked to find room for it twice -- the case a laptop
    /// with a full-ish drive hits on its second launch.
    #[test]
    fn a_runtime_already_on_disk_is_not_charged_for_again() {
        let pinned: GameDescriptor = serde_json::from_value(json!({
            "id": "rust", "name": "Rust", "hosts": ["win32-x64"],
            "requires": { "ramMb": 8192, "diskMb": 16000 },
            "ready": { "marker": "up", "timeoutMs": 1000 },
            "console": { "via": "stdin" },
            "stop": { "via": "console", "command": "quit", "graceMs": 1000 },
            "saves": { "paths": ["world"] },
            "platforms": { "win32-x64": {
                "runtime": { "source": "steamcmd", "appId": 258550, "buildId": "1928" },
                "launch": { "exe": "RustDedicated.exe" } } }
        }))
        .unwrap();
        let cramped = Machine {
            disk_mb: 2000,
            ..capable()
        };

        let fresh = doctor(&pinned, &cramped, &NOTHING, true);
        assert!(!fresh.ok, "a fresh install needs the space");

        let present = fetch::Present {
            suspect: false,
            build_id: Some("1928".into()),
        };
        let again = doctor(&pinned, &cramped, &present, true);
        assert!(again.ok, "{:?}", again.problems);
        assert!(again.runtime_present);
    }

    #[test]
    fn a_thin_processor_is_a_warning_rather_than_a_refusal() {
        let d: GameDescriptor = serde_json::from_value(json!({
            "id": "rust", "name": "Rust", "hosts": ["win32-x64"],
            "requires": { "ramMb": 1, "diskMb": 1, "cpuCores": 8 },
            "ready": { "marker": "up", "timeoutMs": 1 },
            "console": { "via": "stdin" },
            "stop": { "via": "console", "command": "quit", "graceMs": 1 },
            "saves": { "paths": ["world"] },
            "platforms": { "win32-x64": {
                "runtime": { "source": "steamcmd", "appId": 1 },
                "launch": { "exe": "S.exe" } } }
        }))
        .unwrap();
        let v = doctor(
            &d,
            &Machine {
                cpu_cores: Some(2),
                ..capable()
            },
            &NOTHING,
            true,
        );
        assert!(v.ok, "it still starts: {:?}", v.problems);
        assert!(
            v.warnings.iter().any(|w| w.contains("struggle")),
            "{:?}",
            v.warnings
        );
    }

    /// The API's one-service limit, reported as the platform gap it is.
    #[test]
    fn a_game_needing_two_port_groups_warns_rather_than_refusing() {
        let d: GameDescriptor = serde_json::from_value(json!({
            "id": "g", "name": "G", "hosts": ["win32-x64"],
            "ready": { "marker": "up", "timeoutMs": 1 },
            "console": { "via": "stdin" },
            "stop": { "via": "console", "command": "stop", "graceMs": 1 },
            "ports": [
                { "name": "game", "proto": "udp", "port": 1, "expose": true, "service": "game" },
                { "name": "web",  "proto": "tcp", "port": 2, "expose": true, "service": "web" }
            ],
            "saves": { "paths": ["world"] },
            "platforms": { "win32-x64": {
                "runtime": { "source": "direct", "url": "https://x/y",
                             "sha256": "aa" },
                "launch": { "exe": "S.exe" } } }
        }))
        .unwrap();
        let v = doctor(&d, &capable(), &NOTHING, true);
        assert!(
            v.warnings.iter().any(|w| w.contains("one group")),
            "{:?}",
            v.warnings
        );
    }

    /// A broken descriptor is reported as itself, not as its consequences.
    #[test]
    fn a_descriptor_that_does_not_describe_a_game_says_so_first() {
        let v = doctor(&GameDescriptor::default(), &capable(), &NOTHING, true);
        assert!(!v.ok);
        assert!(!v.problems.is_empty());
    }

    #[test]
    fn runtime_working_directory_surfaces_the_save_warning() {
        let mut d = rust();
        for p in d.platforms.values_mut() {
            p.launch.cwd_base = super::super::descriptor::CwdBase::Runtime;
        }
        let verdict = doctor(&d, &capable(), &NOTHING, true);
        assert!(
            verdict
                .warnings
                .iter()
                .any(|w| w.contains("no save mounts")),
            "doctor must warn that runtime-relative saves need redirection"
        );
    }

    #[test]
    fn sizes_read_the_way_a_person_would_say_them() {
        assert_eq!(gigabytes(512), "512 MB");
        assert_eq!(gigabytes(8192), "8 GB");
        assert_eq!(gigabytes(16000), "15.6 GB");
    }
}
