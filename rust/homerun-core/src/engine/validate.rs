//! Is this a descriptor that describes a game?
//!
//! # Everything at once, not the first thing
//!
//! Unlike [`super::settings`], this collects. Its audience is whoever is
//! writing a descriptor — an onboarding session, a reviewer, CI — and telling
//! them one fault at a time turns a five-minute fix into five round trips
//! through a nine-gigabyte download.
//!
//! # Problems and warnings
//!
//! A **problem** means the descriptor cannot be run. A **warning** means it
//! can, and something about it is worse than whoever wrote it probably
//! intended. [`super::doctor`] carries both through to its verdict, where
//! warnings become the YELLOW half of a dossier.
//!
//! # The checks that are about safety rather than correctness
//!
//! Three of these refuse things that would otherwise work, and each is here
//! because working is not the same as safe:
//!
//!  - **`exe` may not be templated.** A setting is player data, so
//!    `"exe": "{setting:binary}"` would let whoever creates a server choose
//!    which file Homerun executes.
//!  - **A join URL may not carry a secret.** It is in the servable half: the
//!    API hands it to any UI, so a `{secret:rcon}` in it publishes the
//!    server's admin password.
//!  - **An RCON port may not be exposed.** Exposing it puts an
//!    administrative console on the public internet behind one password.
//!
//! # Paths
//!
//! Every path in a descriptor is relative and stays inside the directory it
//! is relative to. An absolute path or a `..` is refused — a descriptor is
//! data that decides what a machine writes, and the file a game is asked to
//! create should not be able to be `C:\Windows\System32\drivers\etc\hosts`.

use std::collections::{BTreeSet, HashSet};

use super::descriptor::{
    ConsoleVia, GameDescriptor, PlayersVia, Setting, SettingKind, StopVia, SCHEMA_VERSION,
};
use super::settings::SERVER_NAME_PLACEHOLDER;
use super::template::{self, Placeholder};
use crate::tunnel::Protocol;
use serde_json::Value;

/// What is wrong with a descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    pub problems: Vec<String>,
    pub warnings: Vec<String>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Where a templated string appears, which decides what may appear in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Site {
    /// argv, env, config values: settings, ports, secrets, name, dir.
    Host,
    /// `client.joinUrl`: the gateway address and ports, and nothing private.
    Servable,
}

/// Judge a descriptor.
pub fn report(descriptor: &GameDescriptor) -> Report {
    let mut r = Report::default();

    check_identity(descriptor, &mut r);
    check_settings(descriptor, &mut r);
    check_ports(descriptor, &mut r);
    check_lifecycle(descriptor, &mut r);
    check_platforms(descriptor, &mut r);
    check_config_and_saves(descriptor, &mut r);
    check_servable(descriptor, &mut r);
    check_observe(descriptor, &mut r);

    r
}

/// Every `{secret:…}` this descriptor needs, so a host knows what to generate.
///
/// Secrets are not declared anywhere — they are generated per server, stored
/// by the host and never sent to the API — so the descriptor's own use of
/// them is the only list there is.
pub fn required_secrets(descriptor: &GameDescriptor) -> Vec<String> {
    let mut names = BTreeSet::new();
    for string in templated_strings(descriptor) {
        if let Ok(found) = template::placeholders(&string) {
            for p in found {
                if let Placeholder::Secret(name) = p {
                    names.insert(name);
                }
            }
        }
    }
    // A console secret is needed whether or not anything templates it: the
    // supervisor authenticates with it.
    if descriptor.console.via == ConsoleVia::Rcon {
        if let Some(rcon) = &descriptor.console.rcon {
            if !rcon.secret.is_empty() {
                names.insert(rcon.secret.clone());
            }
        }
    }
    names.into_iter().collect()
}

/// Every host-side templated string, for the scans above.
fn templated_strings(descriptor: &GameDescriptor) -> Vec<String> {
    let mut out = Vec::new();
    for platform in descriptor.platforms.values() {
        out.extend(platform.launch.args.iter().cloned());
        out.extend(platform.launch.env.values().cloned());
    }
    for file in &descriptor.config {
        out.extend(file.keys.values().cloned());
    }
    out
}

fn check_identity(d: &GameDescriptor, r: &mut Report) {
    if d.schema > SCHEMA_VERSION {
        r.problems.push(format!(
            "this game's descriptor is written for a newer version of Homerun \
             (it says schema {}, this build reads {SCHEMA_VERSION}).",
            d.schema
        ));
    }

    if d.id.is_empty() {
        r.problems.push("this game's descriptor has no id.".into());
    } else if !d
        .id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        r.problems.push(format!(
            "\"{}\" is not a usable game id: only lower-case letters, digits and \
             hyphens.",
            d.id
        ));
    }

    if d.name.is_empty() {
        r.problems
            .push("this game's descriptor has no name to show a player.".into());
    }

    if d.hosts.is_empty() {
        r.problems
            .push("this game's descriptor does not say which computers can host it.".into());
    }

    for host in &d.hosts {
        if !d.platforms.contains_key(host) {
            r.problems.push(format!(
                "this game says it runs on {host} but does not say how to install or \
                 start it there."
            ));
        }
    }
    for host in d.platforms.keys() {
        if !d.hosts.contains(host) {
            r.problems.push(format!(
                "this game describes how to run on {host} but does not list it as a \
                 computer it supports. One of the two is out of date."
            ));
        }
    }

    let accent = &d.catalog.accent;
    if !accent.is_empty()
        && !(accent.len() == 7
            && accent.starts_with('#')
            && accent[1..].chars().all(|c| c.is_ascii_hexdigit()))
    {
        r.problems.push(format!(
            "\"{accent}\" is not a colour Homerun can show; it has to look like \
             #CD412B."
        ));
    }

    if let Some(licence) = &d.licence {
        if licence.name.is_empty() || licence.url.is_empty() {
            r.problems.push(
                "this game has terms to accept but does not say what they are called \
                 or where to read them."
                    .into(),
            );
        }
        if let Some(via) = &licence.accept_via {
            check_path(
                &via.file,
                "the file this game wants its acceptance written to",
                r,
            );
        }
    }
}

fn check_settings(d: &GameDescriptor, r: &mut Report) {
    let mut seen = HashSet::new();
    for setting in &d.settings {
        if setting.key.is_empty() {
            r.problems
                .push("one of this game's settings has no key.".into());
            continue;
        }
        if !seen.insert(&setting.key) {
            r.problems.push(format!(
                "this game declares the setting \"{}\" more than once.",
                setting.key
            ));
        }
        if setting.label.is_empty() {
            r.warnings.push(format!(
                "the setting \"{}\" has no label, so a player will see its key.",
                setting.key
            ));
        }

        if let (Some(min), Some(max)) = (setting.min, setting.max) {
            if min > max {
                r.problems.push(format!(
                    "the setting \"{}\" allows {min} at the least and {max} at the \
                     most, which is impossible.",
                    setting.key
                ));
            }
        }
        if setting.kind != SettingKind::Int && (setting.min.is_some() || setting.max.is_some()) {
            r.warnings.push(format!(
                "the setting \"{}\" is not a number, so its smallest and largest \
                 values are ignored.",
                setting.key
            ));
        }

        check_default(setting, r);
    }
}

/// A default is descriptor-authored, so it may carry `{serverName}` — and
/// nothing else.
///
/// Allowing more would route a value into player-visible settings by a
/// different door than [`super::template`] guards. See
/// [`super::settings::default_for`] for where the one allowance is spent.
fn check_default(setting: &Setting, r: &mut Report) {
    let Value::String(text) = &setting.default else {
        // A non-string default cannot carry a placeholder at all.
        if let Some(value) = setting.default.as_i64() {
            if let (Some(min), Some(max)) = (setting.min, setting.max) {
                if value < min || value > max {
                    r.problems.push(format!(
                        "the setting \"{}\" defaults to {value}, which is outside the \
                         {min} to {max} it allows.",
                        setting.key
                    ));
                }
            }
        }
        return;
    };

    let stripped = text.replace(SERVER_NAME_PLACEHOLDER, "");
    match template::placeholders(&stripped) {
        Ok(found) if found.is_empty() => {}
        Ok(found) => r.problems.push(format!(
            "the setting \"{}\" defaults to {}, and a setting's default may only \
             use {SERVER_NAME_PLACEHOLDER}.",
            setting.key,
            found
                .iter()
                .map(Placeholder::spelling)
                .collect::<Vec<_>>()
                .join(" ")
        )),
        Err(err) => r.problems.push(format!(
            "the setting \"{}\" has a default Homerun cannot read: {err}",
            setting.key
        )),
    }

    if !setting.options.is_empty() && !setting.options.contains(&setting.default) {
        r.problems.push(format!(
            "the setting \"{}\" defaults to something that is not one of the choices \
             it offers.",
            setting.key
        ));
    }
}

fn check_ports(d: &GameDescriptor, r: &mut Report) {
    let mut seen = HashSet::new();
    for port in &d.ports {
        if port.name.is_empty() {
            r.problems
                .push("one of this game's ports has no name.".into());
            continue;
        }
        if !seen.insert(&port.name) {
            r.problems.push(format!(
                "this game declares the port \"{}\" twice.",
                port.name
            ));
        }
        if port.port == 0 {
            r.problems.push(format!(
                "the port \"{}\" has no number, so nothing can reach it.",
                port.name
            ));
        }
    }
}

fn check_lifecycle(d: &GameDescriptor, r: &mut Report) {
    if d.ready.marker.is_empty() {
        r.problems.push(
            "this game's descriptor does not say what its server prints when it has \
             finished starting, so Homerun would never know it was up."
                .into(),
        );
    }
    if d.ready.timeout_ms == 0 {
        r.warnings.push(
            "this game does not say how long its server may take to start; Homerun \
             will allow fifteen minutes."
                .into(),
        );
    }

    match d.console.via {
        ConsoleVia::Rcon => match &d.console.rcon {
            None => r.problems.push(
                "this game's console is RCON, but the descriptor does not say which \
                 port or password to use."
                    .into(),
            ),
            Some(rcon) => {
                match d.port(&rcon.port) {
                    None => r.problems.push(format!(
                        "this game's console uses a port called \"{}\", which it does \
                         not declare.",
                        rcon.port
                    )),
                    Some(port) => {
                        if port.expose {
                            r.problems.push(format!(
                                "the port \"{}\" carries this game's administrative \
                                 console and must not be published to the internet.",
                                port.name
                            ));
                        }
                        if port.proto != Protocol::Tcp {
                            r.warnings.push(format!(
                                "the port \"{}\" carries RCON, which is normally TCP.",
                                port.name
                            ));
                        }
                    }
                }
                if rcon.secret.is_empty() {
                    r.problems.push(
                        "this game's console is RCON but the descriptor names no \
                         password for it."
                            .into(),
                    );
                }
            }
        },
        ConsoleVia::Stdin | ConsoleVia::None => {
            if d.console.rcon.is_some() {
                r.warnings
                    .push("this game describes an RCON connection it does not use.".into());
            }
        }
    }

    match d.stop.via {
        StopVia::Console => {
            if d.stop.command.as_deref().unwrap_or_default().is_empty() {
                r.problems.push(
                    "this game is stopped through its console but the descriptor does \
                     not say what to send."
                        .into(),
                );
            }
            if d.console.via == ConsoleVia::None {
                r.problems
                    .push("this game is stopped through its console and has no console.".into());
            }
        }
        StopVia::Interrupt => {
            r.warnings.push(
                "this game can only be stopped by interrupting it, which Windows \
                 cannot always do cleanly; a save may be lost on shutdown."
                    .into(),
            );
        }
    }

    if d.stop.grace_ms == 0 {
        r.warnings.push(
            "this game does not say how long its server may take to shut down; \
             Homerun will allow thirty seconds before insisting."
                .into(),
        );
    }
}

fn check_platforms(d: &GameDescriptor, r: &mut Report) {
    let setting_keys: HashSet<&str> = d.settings.iter().map(|s| s.key.as_str()).collect();
    let port_names: HashSet<&str> = d.ports.iter().map(|p| p.name.as_str()).collect();

    for (host, platform) in &d.platforms {
        let launch = &platform.launch;
        if launch.exe.is_empty() {
            r.problems
                .push(format!("this game does not say what to run on {host}."));
        } else if launch.exe.contains('{') {
            // See the module header: player data must never decide which
            // file is executed.
            r.problems.push(format!(
                "this game's {host} descriptor builds the name of the program to run \
                 out of a setting, which Homerun does not allow."
            ));
        } else {
            check_path(&launch.exe, &format!("the program to run on {host}"), r);
        }

        if let Some(cwd) = &launch.cwd {
            check_path(cwd, &format!("the working directory on {host}"), r);
        }

        for arg in &launch.args {
            check_placeholders(arg, Site::Host, &setting_keys, &port_names, r);
        }
        for value in launch.env.values() {
            check_placeholders(value, Site::Host, &setting_keys, &port_names, r);
        }

        if platform.runtime.source == super::descriptor::RuntimeSource::Direct {
            match platform.runtime.sha256.as_deref() {
                None => r.problems.push(format!(
                    "this game's {host} download has no checksum, so Homerun cannot \
                     tell whether what arrives is what was meant."
                )),
                Some(digest)
                    if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) =>
                {
                    r.problems.push(format!(
                        "this game's {host} download has a checksum that is not a \
                         sha256."
                    ))
                }
                Some(_) => {}
            }
            if platform.runtime.url.is_none() {
                r.problems
                    .push(format!("this game's {host} download has no address."));
            }
        }
    }
}

fn check_config_and_saves(d: &GameDescriptor, r: &mut Report) {
    let setting_keys: HashSet<&str> = d.settings.iter().map(|s| s.key.as_str()).collect();
    let port_names: HashSet<&str> = d.ports.iter().map(|p| p.name.as_str()).collect();

    for file in &d.config {
        check_path(&file.file, "a configuration file", r);
        for value in file.keys.values() {
            check_placeholders(value, Site::Host, &setting_keys, &port_names, r);
        }
    }

    for path in &d.saves.paths {
        check_path(path, "a saved game", r);
    }
    if d.saves.paths.is_empty() {
        r.warnings.push(
            "this game does not say where it keeps saved games, so nothing of it will \
             be backed up."
                .into(),
        );
    }
}

fn check_servable(d: &GameDescriptor, r: &mut Report) {
    let setting_keys: HashSet<&str> = d.settings.iter().map(|s| s.key.as_str()).collect();
    let port_names: HashSet<&str> = d.ports.iter().map(|p| p.name.as_str()).collect();

    if let Some(url) = &d.client.join_url {
        check_placeholders(url, Site::Servable, &setting_keys, &port_names, r);
    }
}

fn check_observe(d: &GameDescriptor, r: &mut Report) {
    match d.observe.players {
        PlayersVia::Rcon => {
            if d.console.via != ConsoleVia::Rcon {
                r.problems.push(
                    "this game counts its players over RCON but its console is not \
                     RCON."
                        .into(),
                );
            }
            if d.observe
                .players_command
                .as_deref()
                .unwrap_or_default()
                .is_empty()
            {
                r.problems.push(
                    "this game counts its players over RCON but the descriptor does \
                     not say what to ask."
                        .into(),
                );
            }
        }
        PlayersVia::LogRegex => {
            let named = d
                .observe
                .presence
                .as_ref()
                .map(|p| !p.join.is_empty() && !p.leave.is_empty())
                .unwrap_or(false);
            if !named {
                r.problems.push(
                    "this game counts its players from its log but the descriptor does \
                     not say which lines mean someone joined or left."
                        .into(),
                );
            }
        }
        PlayersVia::A2s => {
            if !d.ports.iter().any(|p| p.proto == Protocol::Udp) {
                r.warnings.push(
                    "this game is queried over A2S, which needs a UDP port, and it \
                     declares none."
                        .into(),
                );
            }
        }
        PlayersVia::None => {
            r.warnings.push("this game reports no player count.".into());
        }
    }
}

/// Check one templated string against what may appear where it appears.
fn check_placeholders(
    text: &str,
    site: Site,
    settings: &HashSet<&str>,
    ports: &HashSet<&str>,
    r: &mut Report,
) {
    let found = match template::placeholders(text) {
        Ok(found) => found,
        Err(err) => {
            r.problems.push(err.to_string());
            return;
        }
    };

    for placeholder in found {
        match (&placeholder, site) {
            (Placeholder::Setting(key), _) if !settings.contains(key.as_str()) => {
                r.problems.push(format!(
                    "\"{text}\" uses a setting called \"{key}\" that this game does \
                     not declare."
                ));
            }
            (Placeholder::Port(name), _) if !ports.contains(name.as_str()) => {
                r.problems.push(format!(
                    "\"{text}\" uses a port called \"{name}\" that this game does not \
                     declare."
                ));
            }
            // The servable half reaches any UI. See the module header.
            (Placeholder::Secret(name), Site::Servable) => {
                r.problems.push(format!(
                    "this game's join address contains its \"{name}\" password, which \
                     would publish it to everyone who can see the server."
                ));
            }
            (Placeholder::Setting(_), Site::Servable) => {
                r.problems.push(format!(
                    "\"{text}\" is shown to players and cannot depend on a setting."
                ));
            }
            (Placeholder::Host, Site::Host) => {
                r.problems.push(format!(
                    "\"{text}\" uses the address players connect to, which is known \
                     only to Homerun's servers and not to this computer."
                ));
            }
            _ => {}
        }
    }
}

/// Relative, and staying inside where it is relative to.
fn check_path(path: &str, what: &str, r: &mut Report) {
    let looks_absolute = path.starts_with('/')
        || path.starts_with('\\')
        || (path.len() > 1 && path.as_bytes()[1] == b':');

    if looks_absolute {
        r.problems.push(format!(
            "{what} is given as \"{path}\", and it has to be inside the server's own \
             folder."
        ));
    } else if path.split(['/', '\\']).any(|part| part == "..") {
        r.problems.push(format!(
            "{what} is given as \"{path}\", which points outside the server's own \
             folder."
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rust() -> GameDescriptor {
        GameDescriptor::parse(include_str!("testdata/rust.json")).unwrap()
    }

    /// The pilot is the fixture every other module is tested against, so it
    /// had better be a descriptor this one accepts.
    #[test]
    fn the_pilot_is_valid() {
        let r = report(&rust());
        assert!(r.ok(), "{:#?}", r.problems);
    }

    fn problems_of(patch: serde_json::Value) -> Vec<String> {
        let mut base: serde_json::Value =
            serde_json::from_str(include_str!("testdata/rust.json")).unwrap();
        deep_merge(&mut base, &patch);
        let d: GameDescriptor = serde_json::from_value(base).unwrap();
        report(&d).problems
    }

    fn deep_merge(target: &mut serde_json::Value, patch: &serde_json::Value) {
        match (target, patch) {
            (serde_json::Value::Object(t), serde_json::Value::Object(p)) => {
                for (k, v) in p {
                    deep_merge(t.entry(k.clone()).or_insert(serde_json::Value::Null), v);
                }
            }
            (t, p) => *t = p.clone(),
        }
    }

    fn says(problems: &[String], needle: &str) -> bool {
        problems.iter().any(|p| p.contains(needle))
    }

    // ─── the three safety checks ───────────────────────────────────────────

    /// Player data must never decide which file is executed.
    #[test]
    fn a_templated_executable_is_refused() {
        let p = problems_of(json!({
            "platforms": { "win32-x64": { "launch": { "exe": "{setting:hostname}.exe" } } }
        }));
        assert!(says(&p, "does not allow"), "{p:#?}");
    }

    /// The join URL is served to any UI, so a secret in it is published.
    #[test]
    fn a_password_in_the_join_address_is_refused() {
        let p = problems_of(json!({
            "client": { "joinUrl": "steam://connect/{host}:{port:game}?p={secret:rcon}" }
        }));
        assert!(says(&p, "password"), "{p:#?}");
        assert!(says(&p, "publish"), "{p:#?}");
    }

    #[test]
    fn an_exposed_administrative_console_is_refused() {
        let p = problems_of(json!({
            "ports": [
                { "name": "game", "proto": "udp", "port": 28015, "expose": true, "service": "game" },
                { "name": "query", "proto": "udp", "port": 28017, "expose": true, "service": "game" },
                { "name": "rcon", "proto": "tcp", "port": 28016, "expose": true }
            ]
        }));
        assert!(says(&p, "must not be published"), "{p:#?}");
    }

    // ─── paths ─────────────────────────────────────────────────────────────

    #[test]
    fn a_path_that_leaves_the_server_folder_is_refused() {
        assert!(says(
            &problems_of(json!({ "saves": { "paths": ["../../etc"] } })),
            "outside"
        ));
        assert!(says(
            &problems_of(json!({ "config": [{ "file": "C:\\Windows\\hosts", "format": "ini" }] })),
            "inside the server's own folder"
        ));
        assert!(says(
            &problems_of(json!({ "platforms": { "win32-x64": { "launch": { "cwd": "/etc" } } } })),
            "inside the server's own folder"
        ));
    }

    // ─── placeholders ──────────────────────────────────────────────────────

    #[test]
    fn an_argument_naming_a_setting_that_does_not_exist_is_refused() {
        let p = problems_of(json!({
            "platforms": { "win32-x64": { "launch": {
                "args": ["+server.hostname", "{setting:nickname}"] } } }
        }));
        assert!(says(&p, "nickname"), "{p:#?}");
    }

    #[test]
    fn an_argument_naming_a_port_that_does_not_exist_is_refused() {
        let p = problems_of(json!({
            "platforms": { "win32-x64": { "launch": { "args": ["{port:companion}"] } } }
        }));
        assert!(says(&p, "companion"), "{p:#?}");
    }

    #[test]
    fn an_unknown_placeholder_is_refused_rather_than_ignored() {
        let p = problems_of(json!({
            "platforms": { "win32-x64": { "launch": { "args": ["{waffle}"] } } }
        }));
        assert!(says(&p, "{waffle}"), "{p:#?}");
    }

    #[test]
    fn the_gateway_address_cannot_appear_in_a_launch_line() {
        let p = problems_of(json!({
            "platforms": { "win32-x64": { "launch": { "args": ["--advertise", "{host}"] } } }
        }));
        assert!(says(&p, "only to Homerun's servers"), "{p:#?}");
    }

    // ─── settings ──────────────────────────────────────────────────────────

    #[test]
    fn a_default_may_only_use_the_server_name() {
        let p = problems_of(json!({
            "settings": [{ "key": "hostname", "type": "string", "label": "N",
                           "default": "{secret:rcon}" }]
        }));
        assert!(says(&p, "may only use"), "{p:#?}");
    }

    #[test]
    fn a_default_outside_its_own_bounds_is_refused() {
        let p = problems_of(json!({
            "settings": [{ "key": "maxPlayers", "type": "int", "label": "M",
                           "default": 500, "min": 1, "max": 200 }]
        }));
        assert!(says(&p, "outside"), "{p:#?}");
    }

    #[test]
    fn impossible_bounds_are_refused() {
        let p = problems_of(json!({
            "settings": [{ "key": "x", "type": "int", "label": "X", "default": 1,
                           "min": 10, "max": 2 }]
        }));
        assert!(says(&p, "impossible"), "{p:#?}");
    }

    #[test]
    fn a_setting_declared_twice_is_refused() {
        let p = problems_of(json!({
            "settings": [
                { "key": "a", "type": "int", "label": "A", "default": 1 },
                { "key": "a", "type": "int", "label": "A", "default": 2 }
            ]
        }));
        assert!(says(&p, "more than once"), "{p:#?}");
    }

    // ─── identity and lifecycle ────────────────────────────────────────────

    #[test]
    fn a_descriptor_from_a_newer_schema_is_refused_rather_than_half_read() {
        assert!(says(&problems_of(json!({ "schema": 1 })), "newer version"));
    }

    #[test]
    fn a_game_that_says_it_runs_somewhere_it_cannot_be_installed_is_refused() {
        let p = problems_of(json!({ "hosts": ["win32-x64", "linux-x64"] }));
        assert!(says(&p, "linux-x64"), "{p:#?}");
    }

    #[test]
    fn a_game_with_no_ready_marker_is_refused_because_it_would_never_come_up() {
        let p = problems_of(json!({ "ready": { "marker": "" } }));
        assert!(says(&p, "never know it was up"), "{p:#?}");
    }

    #[test]
    fn a_console_stop_with_no_verb_is_refused() {
        let p = problems_of(json!({ "stop": { "via": "console", "command": null } }));
        assert!(says(&p, "what to send"), "{p:#?}");
    }

    #[test]
    fn an_id_that_is_not_a_slug_is_refused() {
        assert!(says(
            &problems_of(json!({ "id": "Rust Game" })),
            "not a usable game id"
        ));
    }

    #[test]
    fn a_colour_that_is_not_one_is_refused() {
        assert!(says(
            &problems_of(json!({ "catalog": { "accent": "red" } })),
            "not a colour"
        ));
    }

    #[test]
    fn counting_players_over_rcon_without_a_command_is_refused() {
        let p = problems_of(json!({ "observe": { "players": "rcon", "playersCommand": null } }));
        assert!(says(&p, "what to ask"), "{p:#?}");
    }

    // ─── warnings ──────────────────────────────────────────────────────────

    #[test]
    fn an_interrupt_only_game_is_a_warning_and_not_a_refusal() {
        let mut base: serde_json::Value =
            serde_json::from_str(include_str!("testdata/rust.json")).unwrap();
        deep_merge(&mut base, &json!({ "stop": { "via": "interrupt" } }));
        let r = report(&serde_json::from_value(base).unwrap());
        assert!(r.ok(), "{:#?}", r.problems);
        assert!(
            r.warnings.iter().any(|w| w.contains("save may be lost")),
            "{:#?}",
            r.warnings
        );
    }

    // ─── secrets ───────────────────────────────────────────────────────────

    /// Secrets are declared nowhere, so what the descriptor uses is the only
    /// list a host has of what to generate.
    #[test]
    fn the_secrets_a_host_must_generate_are_collected_from_the_descriptor() {
        assert_eq!(required_secrets(&rust()), vec!["rcon".to_string()]);
    }

    #[test]
    fn a_console_secret_counts_even_when_nothing_templates_it() {
        let d: GameDescriptor = serde_json::from_value(json!({
            "id": "g",
            "console": { "via": "rcon", "rcon": { "protocol": "source", "port": "rcon",
                                                  "secret": "admin" } }
        }))
        .unwrap();
        assert_eq!(required_secrets(&d), vec!["admin".to_string()]);
    }

    /// Every message here is read by whoever is writing a descriptor, but it
    /// is also carried to a player by `doctor`. None of them may be a
    /// diagnostic.
    #[test]
    fn every_message_reads_as_a_sentence() {
        let r = report(&GameDescriptor::default());
        assert!(!r.problems.is_empty());
        for message in r.problems.iter().chain(r.warnings.iter()) {
            for forbidden in ["unwrap", "panicked", "Err(", "errno", "None"] {
                assert!(!message.contains(forbidden), "{message}");
            }
            assert!(
                message.ends_with('.') || message.ends_with(']'),
                "not a sentence: {message}"
            );
        }
    }
}
