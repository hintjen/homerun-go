//! Running a game server from its descriptor — the decisions half.
//!
//! # What this is
//!
//! One implementation of "fetch, prepare, launch, observe, stop a game
//! server", driven entirely by a `game.json`. Adding a game is a descriptor
//! and a docs page; it is not code. A game the schema cannot express grows
//! the schema, once, and every later game gets the growth.
//!
//! Everything here is pure, as the rest of this crate is: it opens no
//! sockets, spawns nothing, reads no files and has no clock. The effects live
//! in the supervisor crate, and the two are joined by the `engine.*` bridge
//! namespace. That is the same split `crate::launch` and `crate::game`
//! already work on, and it is what lets the whole engine be tested with no
//! game installed.
//!
//! # Why this is not an implementation of [`crate::game::Game`]
//!
//! It would be the obvious move and it is the wrong one. `Game` is frozen at
//! `game/v1` and deliberately excludes artifact resolution — its header says
//! so, and names a Steam depot as the example of why. A descriptor-driven
//! game resolves a Steam depot. Widening the trait to fit would break the
//! Android and iOS hosts at once and silently, because the bridge resolves
//! methods by string at runtime.
//!
//! So this is a sibling of `game`, not a subclass of it: its own module, its
//! own `engine.*` namespace, and no change to `game.*` at all. Minecraft
//! keeps the trait; descriptor-driven games get this.
//!
//! # The order things happen in
//!
//! | Step | Here | What the host does with it |
//! |---|---|---|
//! | Is this descriptor a game? | [`validate::report`] | refuse before anything is downloaded |
//! | May we? | [`licence::gate`] | refuse without a person's recorded acceptance |
//! | Can this machine? | [`doctor::doctor`] | refuse, or warn and continue |
//! | Where does the server come from? | [`fetch::plan`] | download, verify, extract |
//! | What did the player ask for? | [`settings::resolve`] | — |
//! | What is the command line? | [`invocation::compose`] | spawn it |
//! | Is it up yet? | [`control::is_ready`] | stop waiting |
//! | How do I talk to it? | [`control::console`] | open stdin or RCON |
//! | How do I stop it? | [`control::stop_ladder`] | walk the rungs |
//! | How do players reach it? | [`ports::forwards`] | write the wireproxy config |
//!
//! # Three rules that are about safety
//!
//! Each is enforced somewhere below and each exists because the safe
//! behaviour and the working behaviour are not the same:
//!
//!  - **Substitution is single-pass and setting values are never templated**
//!    ([`template`]) — otherwise a player names their server `{secret:rcon}`
//!    and the RCON password becomes the public hostname.
//!  - **A person accepts each game's terms; nothing infers it**
//!    ([`licence`]) — including a session driving the runner, which stops and
//!    asks rather than answering for anyone.
//!  - **steamcmd is anonymous only** ([`fetch`]) — a game needing an account
//!    that owns it is out of scope rather than a credential to find.

pub mod control;
pub mod descriptor;
pub mod doctor;
pub mod fetch;
pub mod invocation;
pub mod licence;
pub mod ports;
pub mod schema;
pub mod settings;
pub mod template;
pub mod validate;

pub use descriptor::{GameDescriptor, SCHEMA_VERSION};

#[cfg(test)]
mod tests {
    const RUNTIME_DIR: &str = "C:/runtime/rust";

    use super::*;
    use serde_json::{json, Map};
    use std::collections::BTreeMap;

    fn rust() -> GameDescriptor {
        GameDescriptor::parse(include_str!("testdata/rust.json")).unwrap()
    }

    /// One pass through the whole module, in the order a launch actually
    /// happens, on the pilot's own descriptor.
    ///
    /// Each step is covered in its own module; this is here because the
    /// interesting failures are at the seams — a resolved setting that the
    /// templater cannot read, a port name the forwards do not know.
    #[test]
    fn the_pilot_goes_from_descriptor_to_command_line() {
        let d = rust();

        // Is this a game?
        assert!(validate::report(&d).ok());

        // May we?
        assert!(licence::gate(&d, false).is_err(), "not without acceptance");
        assert!(licence::gate(&d, true).is_ok());

        // Can this machine?
        let machine = doctor::Machine {
            host: "win32-x64".into(),
            ram_mb: 32768,
            disk_mb: 400_000,
            cpu_cores: Some(16),
        };
        let verdict = doctor::doctor(&d, &machine, &fetch::Present::default(), true);
        assert!(verdict.ok, "{:#?}", verdict.problems);

        // Where from?
        let plan = fetch::plan(&d, "win32-x64", "C:\\rt", &fetch::Present::default()).unwrap();
        assert!(matches!(plan, fetch::Plan::SteamCmd { app_id: 258550, .. }));

        // What did the player ask for?
        let mut asked = Map::new();
        asked.insert("maxPlayers".into(), json!("50"));
        let resolved = settings::resolve(&d, &asked, "Justin's server").unwrap();
        assert_eq!(resolved["maxPlayers"], json!(50));
        assert_eq!(resolved["hostname"], json!("Justin's server"));

        // The host generates what the descriptor asks for, and nothing else.
        assert_eq!(validate::required_secrets(&d), vec!["rcon".to_string()]);

        // What is the command line? The server moved off two preferred ports.
        let bound: BTreeMap<String, u16> = [
            ("game".to_string(), 28015u16),
            ("query".to_string(), 30017),
            ("rcon".to_string(), 30016),
        ]
        .into_iter()
        .collect();
        let secrets: BTreeMap<String, String> = [("rcon".to_string(), "s3cret".to_string())]
            .into_iter()
            .collect();
        let invocation = invocation::compose(
            &d,
            "win32-x64",
            &template::Bindings {
                settings: &resolved,
                ports: &bound,
                secrets: &secrets,
                server_name: "Justin's server",
                server_dir: "C:\\servers\\abc",
                bind_address: "127.0.0.1",
                runtime_dir: RUNTIME_DIR,
            },
        )
        .unwrap();

        assert_eq!(invocation.exe, "RustDedicated.exe");
        let args = invocation.args.join(" ");
        assert!(args.contains("+server.port 28015"), "{args}");
        assert!(
            args.contains("+server.queryport 30017"),
            "the bound port, not 28017: {args}"
        );
        assert!(args.contains("+rcon.password s3cret"), "{args}");
        assert!(args.contains("+server.maxplayers 50"), "{args}");
        assert!(
            !args.contains("+server.seed"),
            "an unset seed takes its flag with it: {args}"
        );

        // Is it up?
        assert!(control::is_ready(&d, "18:22:01 Server startup complete"));

        // How do we talk to it, and stop it?
        assert!(matches!(
            control::console(&d).unwrap(),
            control::ConsoleKind::Rcon { .. }
        ));
        assert_eq!(
            control::stop_ladder(&d)[0].action,
            control::Action::Console {
                command: "quit".into()
            }
        );

        // How do players reach it? RCON stays home.
        let forwards = ports::forwards(&d.ports, &bound).unwrap();
        assert_eq!(forwards.len(), 2);
        assert!(
            forwards.iter().all(|f| f.target_port != 30016),
            "rcon is not published"
        );
    }

    /// The hostile case, end to end rather than per-module: a player who
    /// names their server after the password.
    #[test]
    fn a_server_named_after_the_password_does_not_publish_it() {
        let d = rust();
        let mut asked = Map::new();
        asked.insert("hostname".into(), json!("{secret:rcon}"));
        let resolved = settings::resolve(&d, &asked, "irrelevant").unwrap();

        let bound: BTreeMap<String, u16> = [
            ("game".to_string(), 1u16),
            ("query".to_string(), 2),
            ("rcon".to_string(), 3),
        ]
        .into_iter()
        .collect();
        let secrets: BTreeMap<String, String> = [("rcon".to_string(), "hunter2".to_string())]
            .into_iter()
            .collect();

        let invocation = invocation::compose(
            &d,
            "win32-x64",
            &template::Bindings {
                settings: &resolved,
                ports: &bound,
                secrets: &secrets,
                server_name: "irrelevant",
                server_dir: "C:\\s",
                bind_address: "127.0.0.1",
                runtime_dir: RUNTIME_DIR,
            },
        )
        .unwrap();

        let hostname = invocation
            .args
            .iter()
            .position(|a| a == "+server.hostname")
            .map(|i| invocation.args[i + 1].clone())
            .expect("the hostname argument survives");
        assert_eq!(hostname, "{secret:rcon}", "literal, not resolved");

        // The password appears exactly once: where the descriptor put it.
        let uses = invocation
            .args
            .iter()
            .filter(|a| a.contains("hunter2"))
            .count();
        assert_eq!(uses, 1, "{:?}", invocation.args);
    }
}
