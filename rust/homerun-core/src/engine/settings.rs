//! What a player asked for, turned into what the launch line will carry.
//!
//! # Everything arrives as a string
//!
//! The API validates a server's settings against the registry's copy of the
//! descriptor and stores them in `config.environment_variables` — which is a
//! string map, because every existing env-var code path in the product
//! depends on it being one. So `maxPlayers` reaches this crate as `"10"`,
//! `pve` as `"false"`, and a setting the player left alone as `""`.
//!
//! This module is where that stops being true. It coerces each value to what
//! its declared type says it is, and from here down the engine deals in real
//! integers and booleans.
//!
//! **An empty string and a JSON `null` mean the same thing: unset.** For
//! every type, including `string` — an empty hostname is a hostname the
//! player did not set, and it must drop its flag rather than pass an empty
//! argument to a server that will then advertise itself with a blank name.
//!
//! # Why this re-validates what the API already validated
//!
//! Because the API is not the only caller. `homerun-game launch --settings
//! maxPlayers=9000` reaches exactly this code with no API anywhere in the
//! picture, and a probe on a developer's machine is the one place a bad value
//! is *most* likely. A backstop that only runs when the front door was used
//! is not a backstop.
//!
//! # Why the first problem, rather than all of them
//!
//! [`super::validate`] collects every fault in a descriptor, because its
//! audience is whoever is writing that descriptor and a list is what they
//! want. This is the other case: its audience is a player who set one thing
//! wrong, and the protocol carries one `error.message`. So it stops at the
//! first problem and spends its effort on saying that one well.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::descriptor::{GameDescriptor, Setting, SettingKind};
use crate::{Error, Result};

/// Settings after coercion.
///
/// Every value is `Null`, `String`, an integer `Number`, or `Bool` — never a
/// float, an array or an object. [`resolve`] is the only thing that builds
/// one, and that invariant is what lets [`super::template`] render a value
/// without asking what it might be.
pub type Resolved = BTreeMap<String, Value>;

/// The longest a player's text may be, in characters.
///
/// The same ceiling the API applies. It is not a security boundary — argv,
/// config files and environment blocks all have limits far above this — it is
/// a limit on what a person can plausibly have meant to type.
pub const MAX_TEXT: usize = 256;

/// What a player's text may not be, wherever it lands.
///
/// # Why there is a rule at all, when argv is a list
///
/// Arguments are a `Vec<String>` from here to `Command::args`, so a space in
/// a value cannot split it into two arguments — [`super::invocation`] has a
/// test named for that. It is a real property and it is not enough.
///
/// A dedicated server parses its own argv, and the shapes it parses are not
/// the shapes the operating system passed it:
///
/// - `+key value` parsers — Valve's and Facepunch's — read **one argv
///   element** that happens to be `+rcon.web` as a new switch, not as the
///   value of the switch before it. A server named `+rcon.web` therefore
///   turns on the web console on a server whose player chose the name.
/// - Games that re-read the raw command line — Unreal's `-Key=Value`,
///   Facepunch.CommandLine — parse the string Windows hands them, not the
///   vector Rust built. Rust quotes for MSVCRT's rules, and a parser that is
///   not MSVCRT can be broken out of with a `"`.
/// - A control character reaches a log, a properties file and a console's
///   stdin, and in the last of those a newline is a second command.
///
/// None of that is reachable through a shell, because nothing here uses one.
/// It is reachable through the *game*, which is why the rule is about the
/// value rather than about quoting it.
///
/// # One rule, wherever the value is used
///
/// A value can land in argv, in a config file or in an environment variable,
/// and the same value routinely lands in more than one. Two reasons this is a
/// single rule rather than a stricter one for argv:
///
/// - A setting used in two places would take the strict rule anyway, so a
///   per-site rule buys precision only for a setting used in exactly one —
///   and which one that is changes when a descriptor is edited, with nothing
///   telling the player their name has become illegal.
/// - `{serverName}` is a *Homerun* name that exists before a game is chosen.
///   It has to be judged the same way for every game, so at least one value
///   needs a site-independent rule; having two rules is worse than having the
///   strict one.
///
/// The cost is named rather than hidden: a name may not *begin* with `+`, `-`
/// or `/`, and may not contain `"`. `-=[Clan]=-` is refused and `=[Clan]=-`
/// is not. That is a visible product limit, and the API enforces the same one
/// so a player meets it in a form rather than at a launch that fails.
///
/// A setting with `options` is exempt: its values come from the descriptor,
/// which is ours and signed, not from a player.
pub fn check_text(setting_label: &str, text: &str) -> Result<()> {
    let refuse = |why: &str| {
        Err(Error::Malformed(format!(
            "{setting_label} {why}. Choose something else and try again."
        )))
    };

    if text.chars().count() > MAX_TEXT {
        return refuse(&format!("has to be {MAX_TEXT} characters or fewer"));
    }
    // C0 and DEL. A newline is the one that matters most — it is a second
    // command on a console's stdin and a second key in a properties file —
    // but none of them has a meaning a player intended.
    if text.chars().any(|c| c.is_control()) {
        return refuse("has to be a single line, with no special characters in it");
    }
    if text.contains('"') {
        return refuse("cannot contain a double quote");
    }
    // Matching the API's rule so the two cannot disagree. Nothing here goes
    // near a shell; what these cost a player is nothing, and a value that one
    // layer refuses and the other accepts is a bug waiting for a report.
    if text.contains('`') || text.contains("${") || text.contains("$(") {
        return refuse("cannot contain ` or ${ or $(");
    }
    // A game's own parser reads one of these as the start of a switch,
    // whatever the operating system thought it was handing over.
    if let Some(first) = text.trim_start().chars().next() {
        if matches!(first, '+' | '-' | '/') {
            return refuse("cannot start with +, - or /");
        }
    }
    Ok(())
}

/// The placeholder a setting's `default` is allowed to contain, and the only
/// one.
///
/// See [`super::template`] for why a setting value is never itself templated.
/// A default is descriptor-authored rather than player-authored, so it is not
/// hostile — but allowing `{secret:…}` here would route a secret into a
/// player-visible setting by a different door, so the allowance stops at the
/// server's name.
pub const SERVER_NAME_PLACEHOLDER: &str = "{serverName}";

/// Coerce and check what a player asked for.
///
/// `provided` is what the host has: the API's string map, or the CLI's
/// `--settings k=v` pairs. A key the descriptor does not declare is an error
/// rather than something to carry through — on the CLI path it is almost
/// always a typo, and silently ignoring it produces a server that started
/// without the setting the person was trying to change.
pub fn resolve(
    descriptor: &GameDescriptor,
    provided: &Map<String, Value>,
    server_name: &str,
) -> Result<Resolved> {
    for key in provided.keys() {
        if descriptor.setting(key).is_none() {
            return Err(Error::Malformed(format!(
                "{} has no setting called \"{key}\".",
                display_name(descriptor)
            )));
        }
    }

    // The server's name is player text too, and it reaches a launch line
    // through `{serverName}` and through any default that uses it. It is
    // checked here because `resolve` is the one call every path makes before
    // a value becomes an argument -- the API's, and the CLI's, which has no
    // API in front of it at all.
    check_text("the server's name", server_name)?;

    let mut out = Resolved::new();
    for setting in &descriptor.settings {
        let raw = provided.get(&setting.key);
        let value = match raw {
            Some(v) => coerce(setting, v)?,
            None => default_for(setting, server_name)?,
        };
        if !value.is_null() {
            check_bounds(setting, &value)?;
            // A closed set is descriptor-authored, so its values are ours.
            if let (Value::String(text), true) = (&value, setting.options.is_empty()) {
                check_text(&label(setting), text)?;
            }
        }
        out.insert(setting.key.clone(), value);
    }
    Ok(out)
}

/// The value a setting takes when nobody set it.
///
/// `{serverName}` is substituted here rather than in [`super::template`]
/// because this is the last moment the value is still ours. After this it is
/// player data, and player data is never scanned for placeholders.
fn default_for(setting: &Setting, server_name: &str) -> Result<Value> {
    match &setting.default {
        Value::Null => Ok(Value::Null),
        Value::String(text) => {
            let filled = text.replace(SERVER_NAME_PLACEHOLDER, server_name);
            if filled.is_empty() {
                return Ok(Value::Null);
            }
            coerce(setting, &Value::String(filled))
        }
        other => coerce(setting, other),
    }
}

/// One value, as its declared type.
fn coerce(setting: &Setting, raw: &Value) -> Result<Value> {
    // Unset, however it was spelled. Checked before the type, because it is
    // the same answer for every type.
    if raw.is_null() || raw.as_str() == Some("") {
        return Ok(Value::Null);
    }

    match setting.kind {
        SettingKind::Int => match raw {
            Value::Number(n) if n.is_i64() => Ok(Value::from(n.as_i64().unwrap())),
            Value::String(text) => text
                .trim()
                .parse::<i64>()
                .map(Value::from)
                .map_err(|_| whole_number(setting, text)),
            other => Err(whole_number(setting, &render(other))),
        },
        SettingKind::Bool => match raw {
            Value::Bool(b) => Ok(Value::Bool(*b)),
            Value::Number(n) if n.as_i64() == Some(0) => Ok(Value::Bool(false)),
            Value::Number(n) if n.as_i64() == Some(1) => Ok(Value::Bool(true)),
            Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => Ok(Value::Bool(true)),
                "false" | "0" | "no" | "off" => Ok(Value::Bool(false)),
                _ => Err(yes_or_no(setting, text)),
            },
            other => Err(yes_or_no(setting, &render(other))),
        },
        // A number or a bool reaching a string setting is the CLI, not the
        // API. Render it rather than refusing something that has an obvious
        // reading.
        SettingKind::String => Ok(Value::String(render(raw))),
    }
}

fn check_bounds(setting: &Setting, value: &Value) -> Result<()> {
    if !setting.options.is_empty() && !setting.options.contains(value) {
        let legal: Vec<String> = setting.options.iter().map(render).collect();
        return Err(Error::Malformed(format!(
            "{} has to be one of {}; you chose {}.",
            label(setting),
            legal.join(", "),
            render(value)
        )));
    }

    if let Some(n) = value.as_i64() {
        match (setting.min, setting.max) {
            (Some(min), Some(max)) if n < min || n > max => {
                return Err(Error::Malformed(format!(
                    "{} has to be between {min} and {max}; you asked for {n}.",
                    label(setting)
                )))
            }
            (Some(min), None) if n < min => {
                return Err(Error::Malformed(format!(
                    "{} has to be {min} or more; you asked for {n}.",
                    label(setting)
                )))
            }
            (None, Some(max)) if n > max => {
                return Err(Error::Malformed(format!(
                    "{} has to be {max} or less; you asked for {n}.",
                    label(setting)
                )))
            }
            _ => {}
        }
    }
    Ok(())
}

/// How a resolved value appears on a command line or in a config file.
///
/// Booleans are `true`/`false` — the spelling every game in reach uses.
/// A caller that needs `1`/`0` says so in the descriptor's own argument text.
pub fn render(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn label(setting: &Setting) -> String {
    if setting.label.is_empty() {
        setting.key.clone()
    } else {
        setting.label.clone()
    }
}

fn display_name(descriptor: &GameDescriptor) -> String {
    if descriptor.name.is_empty() {
        "This game".to_string()
    } else {
        descriptor.name.clone()
    }
}

fn whole_number(setting: &Setting, given: &str) -> Error {
    Error::Malformed(format!(
        "{} has to be a whole number; you gave \"{given}\".",
        label(setting)
    ))
}

fn yes_or_no(setting: &Setting, given: &str) -> Error {
    Error::Malformed(format!(
        "{} is a yes-or-no setting; you gave \"{given}\".",
        label(setting)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rust() -> GameDescriptor {
        GameDescriptor::parse(include_str!("testdata/rust.json")).unwrap()
    }

    fn provided(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    /// The shape the API actually sends: every value a string.
    #[test]
    fn the_api_sends_strings_and_they_come_back_typed() {
        let got = resolve(
            &rust(),
            &provided(&[
                ("maxPlayers", json!("25")),
                ("pve", json!("true")),
                ("hostname", json!("Ruined Keep")),
            ]),
            "Justin's server",
        )
        .unwrap();

        assert_eq!(got["maxPlayers"], json!(25), "not the string \"25\"");
        assert_eq!(got["pve"], json!(true), "not the string \"true\"");
        assert_eq!(got["hostname"], json!("Ruined Keep"));
    }

    /// The rule that has to hold for every type, not just for `int`.
    #[test]
    fn an_empty_string_is_unset_for_every_type() {
        let got = resolve(
            &rust(),
            &provided(&[
                ("hostname", json!("")),
                ("maxPlayers", json!("")),
                ("pve", json!("")),
                ("seed", json!("")),
            ]),
            "fallback",
        )
        .unwrap();

        for key in ["hostname", "maxPlayers", "pve", "seed"] {
            assert_eq!(got[key], Value::Null, "{key} should read as unset");
        }
    }

    #[test]
    fn a_json_null_and_an_empty_string_are_the_same_answer() {
        let d = rust();
        let empty = resolve(&d, &provided(&[("seed", json!(""))]), "s").unwrap();
        let null = resolve(&d, &provided(&[("seed", Value::Null)]), "s").unwrap();
        assert_eq!(empty["seed"], null["seed"]);
        assert_eq!(empty["seed"], Value::Null);
    }

    #[test]
    fn a_setting_nobody_touched_takes_its_default() {
        let got = resolve(&rust(), &Map::new(), "Justin's server").unwrap();
        assert_eq!(got["maxPlayers"], json!(10));
        assert_eq!(got["worldSize"], json!(3000));
        assert_eq!(got["pve"], json!(false));
        // declared `"default": null`
        assert_eq!(got["seed"], Value::Null);
    }

    /// The one placeholder a default may carry, resolved here because this is
    /// the last moment the value belongs to us rather than to a player.
    #[test]
    fn a_default_of_server_name_becomes_the_server_name() {
        let got = resolve(&rust(), &Map::new(), "Justin's server").unwrap();
        assert_eq!(got["hostname"], json!("Justin's server"));
    }

    /// The backstop earning its keep: the CLI has no API in front of it.
    #[test]
    fn a_value_out_of_range_is_refused_in_words_a_player_can_act_on() {
        let err = resolve(&rust(), &provided(&[("maxPlayers", json!("9000"))]), "s")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Max players"), "names the setting: {err}");
        assert!(err.contains("200"), "names the ceiling: {err}");
        assert!(err.contains("9000"), "names what was asked for: {err}");
        assert!(
            !err.contains("parse") && !err.contains("Err"),
            "reads as a verdict: {err}"
        );
    }

    #[test]
    fn a_word_where_a_number_belongs_is_refused_by_name() {
        let err = resolve(&rust(), &provided(&[("worldSize", json!("huge"))]), "s")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Map size") && err.contains("whole number"),
            "{err}"
        );
    }

    #[test]
    fn the_spellings_of_yes_and_no_that_a_form_or_a_shell_produces_all_work() {
        let d = rust();
        for (given, expected) in [
            (json!("true"), true),
            (json!("TRUE"), true),
            (json!("1"), true),
            (json!("yes"), true),
            (json!(true), true),
            (json!("false"), false),
            (json!("0"), false),
            (json!("off"), false),
            (json!(false), false),
        ] {
            let got = resolve(&d, &provided(&[("pve", given.clone())]), "s").unwrap();
            assert_eq!(got["pve"], json!(expected), "for {given}");
        }
    }

    #[test]
    fn something_that_is_neither_is_refused() {
        let err = resolve(&rust(), &provided(&[("pve", json!("maybe"))]), "s")
            .unwrap_err()
            .to_string();
        assert!(err.contains("PvE") && err.contains("yes-or-no"), "{err}");
    }

    /// Almost always a typo on the CLI, and carrying it through would produce
    /// a server started without the setting the person came to change.
    #[test]
    fn a_setting_this_game_does_not_have_is_refused_rather_than_ignored() {
        let err = resolve(&rust(), &provided(&[("maxPlayerz", json!("10"))]), "s")
            .unwrap_err()
            .to_string();
        assert!(err.contains("maxPlayerz") && err.contains("Rust"), "{err}");
    }

    #[test]
    fn a_closed_set_refuses_a_value_outside_it_and_names_the_choices() {
        let d: GameDescriptor = serde_json::from_str(
            r#"{ "id": "g", "name": "G", "settings": [
                 { "key": "difficulty", "type": "string", "label": "Difficulty",
                   "default": "normal", "options": ["peaceful", "normal", "hard"] } ] }"#,
        )
        .unwrap();
        let err = resolve(&d, &provided(&[("difficulty", json!("nightmare"))]), "s")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("peaceful") && err.contains("nightmare"),
            "{err}"
        );
    }

    // ─── what a player's text may not be ───────────────────────────────────

    /// The attack the rule exists for. argv is a list, so nothing splits —
    /// and a `+key value` parser reads this one element as a new switch and
    /// turns on the web console for a server whose player chose the name.
    #[test]
    fn a_setting_that_is_a_switch_is_refused_rather_than_passed_along() {
        let err = resolve(&rust(), &provided(&[("hostname", json!("+rcon.web"))]), "s")
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot start with"), "{err}");
        assert!(!err.contains("argv") && !err.contains("parse"), "reads for a player: {err}");
    }

    #[test]
    fn the_three_switch_characters_are_all_refused_wherever_the_padding_is() {
        // The last is a non-breaking space, which is padding to a player and
        // padding to `trim_start`, so it must not hide the switch behind it.
        for given in ["+x", "-x", "/x", "  -x", "\u{a0}-x"] {
            let got = resolve(&rust(), &provided(&[("hostname", json!(given))]), "s");
            assert!(
                got.is_err() || !given.trim_start().starts_with(['+', '-', '/']),
                "{given:?} was accepted"
            );
        }
    }

    /// Rust quotes argv for MSVCRT's rules. A game that re-reads the raw
    /// command line with its own parser — Unreal, Facepunch.CommandLine — is
    /// not MSVCRT, and a quote is how it gets broken out of.
    #[test]
    fn a_double_quote_is_refused_because_not_every_parser_is_the_one_rust_quotes_for() {
        let err = resolve(
            &rust(),
            &provided(&[("hostname", json!("Ruined \" Keep"))]),
            "s",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("double quote"), "{err}");
    }

    /// A newline in a value is a second command on a console's stdin and a
    /// second key in a properties file.
    #[test]
    fn a_value_that_is_more_than_one_line_is_refused() {
        for given in ["a\nb", "a\rb", "a\tb", "a\u{0}b"] {
            let err = resolve(&rust(), &provided(&[("hostname", json!(given))]), "s")
                .unwrap_err()
                .to_string();
            assert!(err.contains("single line"), "for {given:?}: {err}");
        }
    }

    #[test]
    fn text_longer_than_the_ceiling_is_refused_and_text_at_it_is_not() {
        let at = "a".repeat(MAX_TEXT);
        assert!(resolve(&rust(), &provided(&[("hostname", json!(at))]), "s").is_ok());
        let over = "a".repeat(MAX_TEXT + 1);
        let err = resolve(&rust(), &provided(&[("hostname", json!(over))]), "s")
            .unwrap_err()
            .to_string();
        assert!(err.contains("256"), "{err}");
    }

    /// The API refuses these, and a value one layer refuses and the other
    /// accepts is a bug waiting for a report. Nothing here goes near a shell.
    #[test]
    fn the_shell_shapes_the_api_refuses_are_refused_here_too() {
        for given in ["a`b", "a${b}", "a$(b)"] {
            assert!(
                resolve(&rust(), &provided(&[("hostname", json!(given))]), "s").is_err(),
                "{given} was accepted"
            );
        }
    }

    /// The server's name reaches a launch line through `{serverName}` and
    /// through any default that uses it, and it is player text like any
    /// other. `resolve` is the one call every path makes first.
    #[test]
    fn the_server_s_own_name_is_held_to_the_same_rule() {
        let err = resolve(&rust(), &Map::new(), "+rcon.web")
            .unwrap_err()
            .to_string();
        assert!(err.contains("server's name"), "names what is wrong: {err}");
        assert!(err.contains("cannot start with"), "{err}");
    }

    /// A closed set's values are descriptor-authored, so the rule that is
    /// about player text does not apply to them.
    #[test]
    fn a_choice_from_the_descriptor_is_exempt_from_the_rule_about_player_text() {
        let d: GameDescriptor = serde_json::from_str(
            r#"{ "id": "g", "name": "G", "settings": [
                 { "key": "mode", "type": "string", "label": "Mode",
                   "default": "-hardcore", "options": ["-hardcore", "normal"] } ] }"#,
        )
        .unwrap();
        let got = resolve(&d, &provided(&[("mode", json!("-hardcore"))]), "s").unwrap();
        assert_eq!(got["mode"], json!("-hardcore"));
    }

    /// An unset value never reaches a launch line at all, so it is not text
    /// to be judged — and judging it would refuse a player who typed nothing.
    #[test]
    fn an_unset_value_is_not_held_to_the_rule() {
        assert!(resolve(&rust(), &provided(&[("hostname", json!(""))]), "s").is_ok());
    }

    /// Ordinary names keep working. The rule costs the first character and
    /// the double quote, and this is the test that says so out loud.
    #[test]
    fn the_names_people_actually_choose_are_still_accepted() {
        for name in [
            "Justin's server",
            "=[Clan]=-",
            "Ruined Keep",
            "サーバー",
            "server #1 (hard)",
            "100% uptime, we promise",
        ] {
            assert!(
                resolve(&rust(), &provided(&[("hostname", json!(name))]), name).is_ok(),
                "{name} was refused"
            );
        }
    }

    /// Every resolved value [`resolve`] produces is one of four JSON shapes, and the
    /// rest of the engine is written against that.
    #[test]
    fn every_resolved_value_is_null_string_integer_or_bool() {
        let got = resolve(&rust(), &Map::new(), "s").unwrap();
        for (key, value) in &got {
            let ok = value.is_null()
                || value.is_string()
                || value.is_boolean()
                || value.as_i64().is_some();
            assert!(ok, "{key} resolved to {value}, which breaks the invariant");
        }
    }
}
