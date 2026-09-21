//! The command line, composed.
//!
//! # What a host gets, and what it still has to do
//!
//! An [`Invocation`] is the argv, env and working directory, with every
//! placeholder resolved. It is deliberately *not* a spawn: `exe` is still
//! relative to the runtime directory and carries no `.exe`, because whether
//! an executable needs a suffix or an exec bit is a platform question and
//! this crate has no platform. The supervisor's platform module answers that
//! immediately before spawning.
//!
//! # `exe` is never templated
//!
//! Every other string here goes through [`super::template`]; `exe` does not,
//! and [`super::validate`] rejects a descriptor that puts a placeholder in
//! it. The reason is the same one that makes substitution single-pass: a
//! setting is player data, and `"exe": "{setting:binary}"` would let whoever
//! creates a server choose which file on their machine Homerun executes.
//! Nothing needs it, so nothing is allowed it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::descriptor::GameDescriptor;
use super::template::{self, Bindings, Filled};
use crate::{Error, Result};

/// A launch, resolved.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Invocation {
    /// Relative to the runtime directory, without a platform suffix.
    pub exe: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Relative to the server directory. `.` is the server directory itself.
    pub cwd: String,
}

/// Compose the launch line for a host.
pub fn compose(descriptor: &GameDescriptor, host: &str, bindings: &Bindings) -> Result<Invocation> {
    let platform = descriptor.platform(host).ok_or_else(|| {
        Error::Unsupported(format!(
            "{} cannot be hosted on this computer.",
            if descriptor.name.is_empty() {
                "This game"
            } else {
                &descriptor.name
            }
        ))
    })?;
    let launch = &platform.launch;

    // Which source token produced each emitted argument, so that dropping an
    // unset value can drop the flag that introduced it -- and only that.
    let mut args: Vec<String> = Vec::new();
    let mut sources: Vec<usize> = Vec::new();

    for (index, token) in launch.args.iter().enumerate() {
        match template::fill(token, bindings)? {
            Filled::Text(text) => {
                args.push(text);
                sources.push(index);
            }
            Filled::Dropped => {
                // `+server.seed {setting:seed}` with no seed loses both
                // tokens. `seed={setting:seed}` loses only itself: there is
                // no flag in front of it, and the argument before it is
                // unrelated.
                //
                // The flag has to be the token *immediately* before this one
                // in the descriptor. `sources.last()` is the last argument
                // still standing, which after an earlier drop is some earlier
                // token entirely: in
                // `["-batchmode", "+a", "{setting:n1}", "{setting:n2}"]` with
                // both settings unset, `{setting:n1}` takes `+a` with it and
                // then `{setting:n2}` finds `-batchmode` sitting at the end
                // and takes that too. The index check is what stops a second
                // drop reaching back past the hole the first one left.
                if template::is_sole_placeholder(token) {
                    if let Some(&previous) = sources.last() {
                        if previous + 1 == index
                            && template::looks_like_flag(&launch.args[previous])
                        {
                            args.pop();
                            sources.pop();
                        }
                    }
                }
            }
        }
    }

    // An environment variable has no equivalent of a dropped flag: a variable
    // set to the empty string is a different thing from one that is not set,
    // and an unset setting means the latter.
    let mut env = BTreeMap::new();
    for (key, value) in &launch.env {
        if let Filled::Text(text) = template::fill(value, bindings)? {
            env.insert(key.clone(), text);
        }
    }

    Ok(Invocation {
        exe: launch.exe.clone(),
        args,
        env,
        cwd: launch.cwd.clone().unwrap_or_else(|| ".".to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    use super::super::settings::Resolved;

    fn descriptor(args: &[&str]) -> GameDescriptor {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        serde_json::from_value(json!({
            "id": "g",
            "name": "G",
            "hosts": ["win32-x64"],
            "platforms": { "win32-x64": {
                "runtime": { "source": "direct", "url": "https://x/y.zip", "sha256": "ab" },
                "launch": { "exe": "Server.exe", "args": args, "cwd": "." }
            }}
        }))
        .unwrap()
    }

    struct Fixture {
        settings: Resolved,
        ports: BTreeMap<String, u16>,
        secrets: BTreeMap<String, String>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                settings: [
                    ("hostname".to_string(), json!("Ruined Keep")),
                    ("maxPlayers".to_string(), json!(25)),
                    ("seed".to_string(), Value::Null),
                ]
                .into_iter()
                .collect(),
                ports: [("game".to_string(), 28016u16)].into_iter().collect(),
                secrets: [("rcon".to_string(), "hunter2".to_string())]
                    .into_iter()
                    .collect(),
            }
        }
        fn bindings(&self) -> Bindings<'_> {
            Bindings {
                settings: &self.settings,
                ports: &self.ports,
                secrets: &self.secrets,
                server_name: "Justin's server",
                server_dir: "C:\\servers\\abc",
                bind_address: "127.0.0.1",
            }
        }
    }

    fn args_of(tokens: &[&str]) -> Vec<String> {
        let f = Fixture::new();
        compose(&descriptor(tokens), "win32-x64", &f.bindings())
            .unwrap()
            .args
    }

    #[test]
    fn placeholders_resolve_in_place() {
        assert_eq!(
            args_of(&[
                "-batchmode",
                "+server.port",
                "{port:game}",
                "+server.hostname",
                "{setting:hostname}"
            ]),
            vec![
                "-batchmode",
                "+server.port",
                "28016",
                "+server.hostname",
                "Ruined Keep"
            ]
        );
    }

    /// The rule from the contract, in the shape the pilot writes it.
    #[test]
    fn an_unset_setting_takes_its_flag_with_it() {
        assert_eq!(
            args_of(&[
                "+server.maxplayers",
                "{setting:maxPlayers}",
                "+server.seed",
                "{setting:seed}"
            ]),
            vec!["+server.maxplayers", "25"]
        );
    }

    #[test]
    fn an_unset_setting_in_the_middle_leaves_what_surrounds_it_alone() {
        assert_eq!(
            args_of(&[
                "-batchmode",
                "+server.seed",
                "{setting:seed}",
                "-nographics"
            ]),
            vec!["-batchmode", "-nographics"]
        );
    }

    /// A mixed token has no flag in front of it to drop, and the argument
    /// before it is unrelated -- removing it would remove something real.
    #[test]
    fn a_mixed_token_drops_only_itself() {
        assert_eq!(
            args_of(&["-batchmode", "seed={setting:seed}"]),
            vec!["-batchmode"]
        );
        assert_eq!(
            args_of(&[
                "+server.maxplayers",
                "{setting:maxPlayers}",
                "seed={setting:seed}"
            ]),
            vec!["+server.maxplayers", "25"]
        );
    }

    /// The precise case the source-token check exists for: a preceding
    /// argument that merely *looks* like a flag because its value is
    /// negative.
    #[test]
    fn a_negative_value_is_not_mistaken_for_the_flag_to_drop() {
        let mut f = Fixture::new();
        f.settings.insert("offset".into(), json!(-5));
        let d = descriptor(&["+world.offset", "{setting:offset}", "{setting:seed}"]);
        let args = compose(&d, "win32-x64", &f.bindings()).unwrap().args;
        assert_eq!(
            args,
            vec!["+world.offset", "-5"],
            "-5 is a value, not a flag"
        );
    }

    /// Two unset settings in a row must not eat an argument that was never
    /// anybody's value.
    ///
    /// The second drop looks at the last argument still standing, and before
    /// the index check that was `-batchmode` — three tokens away in the
    /// descriptor, separated from this one by the hole the first drop left.
    /// A game launched with neither `-batchmode` nor `+a` opens a window on a
    /// headless machine, which is a failure nobody would trace back to a seed
    /// nobody set.
    #[test]
    fn consecutive_unset_settings_each_drop_only_their_own_flag() {
        let mut f = Fixture::new();
        f.settings.insert("n1".into(), Value::Null);
        f.settings.insert("n2".into(), Value::Null);
        let d = descriptor(&["-batchmode", "+a", "{setting:n1}", "{setting:n2}"]);
        let args = compose(&d, "win32-x64", &f.bindings()).unwrap().args;
        assert_eq!(
            args,
            vec!["-batchmode"],
            "+a is n1's flag and goes with it; -batchmode belongs to nobody"
        );
    }

    /// The same reach-back over a longer distance: each pair loses its own
    /// flag, and the trailing value — which never had one — loses nothing.
    ///
    /// By the last token the only argument left standing is the very first
    /// one, five tokens back. Reaching it is the bug; leaving it is the rule.
    #[test]
    fn a_drop_never_reaches_back_past_the_tokens_that_already_went() {
        let mut f = Fixture::new();
        for key in ["n1", "n2", "n3"] {
            f.settings.insert(key.into(), Value::Null);
        }
        let d = descriptor(&[
            "-nographics",
            "+a",
            "{setting:n1}",
            "+b",
            "{setting:n2}",
            "{setting:n3}",
        ]);
        let args = compose(&d, "win32-x64", &f.bindings()).unwrap().args;
        assert_eq!(
            args,
            vec!["-nographics"],
            "+a and +b go with their own values; -nographics is not a value's flag"
        );
    }

    #[test]
    fn a_dropped_first_argument_has_no_flag_to_take_with_it() {
        assert_eq!(
            args_of(&["{setting:seed}", "-batchmode"]),
            vec!["-batchmode"]
        );
    }

    #[test]
    fn an_unset_setting_leaves_its_environment_variable_unset() {
        let f = Fixture::new();
        let d: GameDescriptor = serde_json::from_value(json!({
            "id": "g", "hosts": ["win32-x64"],
            "platforms": { "win32-x64": {
                "runtime": { "source": "direct", "url": "u", "sha256": "ab" },
                "launch": { "exe": "S.exe", "env": {
                    "SEED": "{setting:seed}",
                    "NAME": "{setting:hostname}"
                }}
            }}
        }))
        .unwrap();
        let env = compose(&d, "win32-x64", &f.bindings()).unwrap().env;
        assert_eq!(env.get("NAME").map(String::as_str), Some("Ruined Keep"));
        assert!(
            !env.contains_key("SEED"),
            "an unset setting is not an empty variable"
        );
    }

    #[test]
    fn a_missing_working_directory_is_the_server_directory() {
        let f = Fixture::new();
        let d: GameDescriptor = serde_json::from_value(json!({
            "id": "g", "hosts": ["win32-x64"],
            "platforms": { "win32-x64": {
                "runtime": { "source": "direct", "url": "u", "sha256": "ab" },
                "launch": { "exe": "S.exe" }
            }}
        }))
        .unwrap();
        assert_eq!(compose(&d, "win32-x64", &f.bindings()).unwrap().cwd, ".");
    }

    #[test]
    fn a_host_this_game_does_not_ship_for_is_refused_in_words() {
        let f = Fixture::new();
        let err = compose(&descriptor(&[]), "linux-x64", &f.bindings())
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot be hosted"), "{err}");
        assert!(!err.contains("None"), "reads as a verdict: {err}");
    }

    /// Arguments are a list, never a string. Stated as a test because the
    /// day someone joins them with spaces is the day a server name with a
    /// space in it becomes two arguments.
    #[test]
    fn a_value_with_a_space_stays_one_argument() {
        let args = args_of(&["+server.hostname", "{setting:hostname}"]);
        assert_eq!(args.len(), 2);
        assert_eq!(args[1], "Ruined Keep");
    }
}
