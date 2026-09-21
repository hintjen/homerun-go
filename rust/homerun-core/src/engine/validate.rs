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
    check_bind_address(descriptor, &mut r);
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

        check_options(setting, r);
        check_default(setting, r);
    }
}

/// A closed set is a `string` setting with a non-empty list of strings, and
/// there is no other kind.
///
/// The API, the UI and this crate all had to agree on one spelling for "pick
/// one of these", and they now do: `type: "string"` plus `options`, with no
/// `enum` type anywhere. This is the half of that agreement core is
/// responsible for, and it is a refusal rather than a warning because the
/// alternative is a player being offered choices the launch line cannot carry.
///
/// An `int` or `bool` with options is the shape someone reaches for when they
/// want a small set of numbers; the answer is a `string` setting whose options
/// are `"1"`, `"2"`, `"4"`, because that is what reaches argv either way.
///
/// An empty list is not an error: it is indistinguishable from no list at
/// all, and every reader here already treats it that way.
fn check_options(setting: &Setting, r: &mut Report) {
    if setting.options.is_empty() {
        return;
    }
    if setting.kind != SettingKind::String {
        r.problems.push(format!(
            "the setting \"{}\" offers a fixed set of choices, which only a text \
             setting can do. Declare it as text and write its choices as text.",
            setting.key
        ));
    }
    if let Some(odd) = setting.options.iter().find(|o| !o.is_string()) {
        r.problems.push(format!(
            "the setting \"{}\" offers {odd} as one of its choices, and every choice \
             has to be text.",
            setting.key
        ));
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

    // The descriptor's own half of a default has to satisfy the rule player
    // text satisfies, or the refusal lands on a player who typed nothing.
    // `{serverName}` is stripped first: that part is the player's, and
    // `settings::resolve` checks it on its own account.
    if setting.kind == SettingKind::String && setting.options.is_empty() {
        if let Err(err) = super::settings::check_text(&format!("\"{}\"", setting.key), &stripped) {
            r.problems.push(format!(
                "the setting \"{}\" has a default that Homerun would refuse from a \
                 player: {err}",
                setting.key
            ));
        }
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

/// A game with an administrative console had better be told where to put it.
///
/// `expose: false` says the port stays on this computer, and the runner
/// refuses a launch that binds it wider — but refusing after the fact is a
/// stopped server and a message, where passing `{bindAddress}` is a server
/// that comes up correctly. A descriptor with an RCON console and no
/// `{bindAddress}` anywhere in its launch line is one whose console binds
/// wherever the game feels like, which on most games is every interface.
///
/// A warning rather than a problem: a game may take its bind address from a
/// config file this descriptor writes, or may have no way to be told at all,
/// and in the second case the descriptor is still the best available and the
/// runner's check is what stands behind it.
fn check_bind_address(d: &GameDescriptor, r: &mut Report) {
    if d.console.via != ConsoleVia::Rcon {
        return;
    }
    let used = templated_strings(d).iter().any(|s| {
        template::placeholders(s)
            .map(|found| found.contains(&Placeholder::BindAddress))
            .unwrap_or(false)
    });
    if !used {
        r.warnings.push(
            "this game has an administrative console and nothing in its launch line \
             says which address to bind it to, so the game will choose — and most \
             choose every network interface. Pass {bindAddress} where this game \
             takes a bind address."
                .into(),
        );
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
        check_dropped_flags(host, &launch.args, r);
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

/// Warn where the flag-dropping rule will take a flag that is nobody's.
///
/// [`super::invocation`] drops the token in front of an unset sole
/// placeholder, because `+server.seed {setting:seed}` has to lose both halves
/// or leave a flag with no value after it. The rule cannot tell that pair
/// apart from `-batchmode {setting:seed}`, where `-batchmode` carries no
/// value and the seed is a bare positional — so on every launch with no seed,
/// that descriptor silently loses `-batchmode` too. It is inherent to the
/// rule rather than a bug in it, which is exactly why it should be visible
/// while someone is writing the file instead of when a headless machine opens
/// a window.
///
/// What tells the two apart is the descriptor author's own naming: a flag
/// that introduces a setting almost always names it. So this stays quiet when
/// the flag mentions the setting's key and speaks up when it does not. That
/// is a guess about spelling, which is why it is a warning and never a
/// refusal — the cost of a wrong one is a glance, and the cost of no warning
/// at all is an argument missing from every default launch.
fn check_dropped_flags(host: &str, args: &[String], r: &mut Report) {
    for pair in args.windows(2) {
        let (flag, value) = (&pair[0], &pair[1]);
        if !template::looks_like_flag(flag) || !template::is_sole_placeholder(value) {
            continue;
        }
        let Ok(found) = template::placeholders(value) else {
            continue;
        };
        let Some(Placeholder::Setting(key)) = found.first() else {
            continue;
        };
        if mentions(flag, key) {
            continue;
        }
        r.warnings.push(format!(
            "on {host} this game runs \"{flag}\" in front of \"{}\", so when that \
             setting is not set Homerun leaves out both of them. If \"{flag}\" is a \
             flag in its own right rather than the one \"{key}\" belongs to, give \
             \"{key}\" its own flag or a default so it is never unset.",
            value.trim()
        ));
    }
}

/// Whether a flag token names a setting, ignoring how either is punctuated.
///
/// `+server.maxplayers` names `maxPlayers`; `-batchmode` names nothing.
fn mentions(flag: &str, key: &str) -> bool {
    let squash = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    };
    let key = squash(key);
    !key.is_empty() && squash(flag).contains(&key)
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
            // A fact about this machine, and never about how a player
            // reaches the server: in a join URL it would publish `127.0.0.1`
            // as somewhere to connect to.
            (Placeholder::BindAddress, Site::Servable) => {
                r.problems.push(format!(
                    "\"{text}\" is shown to players and cannot contain the address \
                     the server binds on this computer."
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

    fn warnings_of(patch: serde_json::Value) -> Vec<String> {
        let mut base: serde_json::Value =
            serde_json::from_str(include_str!("testdata/rust.json")).unwrap();
        deep_merge(&mut base, &patch);
        let d: GameDescriptor = serde_json::from_value(base).unwrap();
        report(&d).warnings
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

    // ─── the address the server is told to bind ────────────────────────────

    /// `expose: false` is a promise about where a port goes. A descriptor
    /// with an administrative console that never passes `{bindAddress}`
    /// leaves the game to choose, and most games choose every interface.
    #[test]
    fn an_administrative_console_with_no_bind_address_is_warned_about() {
        let warnings = warnings_of(json!({ "platforms": { "win32-x64": { "launch": {
            "args": ["-batchmode", "+rcon.port", "{port:rcon}"]
        }}}}));
        assert!(
            warnings.iter().any(|w| w.contains("{bindAddress}")),
            "{warnings:#?}"
        );
    }

    /// The pilot passes it, so the warning is not something every descriptor
    /// carries and nobody reads.
    #[test]
    fn the_pilot_tells_its_server_which_address_to_bind() {
        let r = report(&rust());
        assert!(r.ok(), "{:#?}", r.problems);
        assert!(
            !r.warnings.iter().any(|w| w.contains("{bindAddress}")),
            "{:#?}",
            r.warnings
        );
    }

    /// It is a fact about this computer. In a join URL it would publish
    /// `127.0.0.1` as somewhere for a player to connect to.
    #[test]
    fn the_bind_address_cannot_appear_in_a_join_address() {
        let problems = problems_of(json!({
            "client": { "joinUrl": "steam://connect/{bindAddress}:{port:game}" }
        }));
        assert!(says(&problems, "the address the server binds"), "{problems:#?}");
    }

    // ─── one spelling for a closed set ─────────────────────────────────────

    /// The API, the UI and this crate agreed on `type: "string"` plus
    /// `options`, and on there being no second way to say it. A numeric
    /// setting with choices is the shape that would have been that second
    /// way, so core refuses it rather than offering a player choices the
    /// launch line cannot carry.
    #[test]
    fn a_closed_set_on_a_number_or_a_yes_or_no_is_refused() {
        for kind in ["int", "bool"] {
            let problems = problems_of(json!({ "settings": [
                { "key": "tickRate", "type": kind, "label": "Tick rate",
                  "default": 30, "options": [30, 60] }
            ]}));
            assert!(
                says(&problems, "only a text setting"),
                "for {kind}: {problems:#?}"
            );
        }
    }

    #[test]
    fn a_choice_that_is_not_text_is_refused() {
        let problems = problems_of(json!({ "settings": [
            { "key": "difficulty", "type": "string", "label": "Difficulty",
              "default": "normal", "options": ["normal", 3] }
        ]}));
        assert!(says(&problems, "has to be text"), "{problems:#?}");
    }

    /// An empty list is how "no closed set" arrives from a generator that
    /// always writes the key. It is not a fault, and every reader here
    /// already treats it as absent.
    #[test]
    fn an_empty_list_of_choices_is_the_same_as_no_list() {
        let problems = problems_of(json!({ "settings": [
            { "key": "hostname", "type": "string", "label": "Name",
              "default": "x", "options": [] }
        ]}));
        assert!(!says(&problems, "choices"), "{problems:#?}");
    }

    /// Bounds on anything but a number are ignored rather than refused —
    /// a descriptor that carries them still runs — but silently ignoring
    /// them is how someone believes a limit is being enforced.
    #[test]
    fn bounds_on_something_that_is_not_a_number_are_warned_about() {
        let warnings = warnings_of(json!({ "settings": [
            { "key": "hostname", "type": "string", "label": "Name",
              "default": "x", "min": 1, "max": 10 }
        ]}));
        assert!(
            warnings.iter().any(|w| w.contains("not a number")),
            "{warnings:#?}"
        );
    }

    // ─── the flag-dropping rule, made visible ──────────────────────────────

    /// `-batchmode` is a flag in its own right and the seed after it is a
    /// bare positional, so every launch without a seed loses `-batchmode`
    /// as well. The rule cannot tell that from a flag-and-value pair, so
    /// authoring time is where it has to be said.
    #[test]
    fn a_flag_that_is_nobody_s_value_is_warned_about_before_it_disappears() {
        let warnings = warnings_of(json!({ "platforms": { "win32-x64": { "launch": {
            "args": ["-batchmode", "{setting:seed}"]
        }}}}));
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("-batchmode") && w.contains("seed")),
            "{warnings:#?}"
        );
    }

    /// And stays quiet for the pair it cannot be, which is the whole reason
    /// the check reads the descriptor's own naming rather than warning on
    /// every optional setting in every game.
    #[test]
    fn a_flag_that_names_its_setting_is_not_warned_about() {
        let warnings = warnings_of(json!({ "platforms": { "win32-x64": { "launch": {
            "args": ["+server.seed", "{setting:seed}",
                     "+server.maxplayers", "{setting:maxPlayers}"]
        }}}}));
        assert!(
            !warnings.iter().any(|w| w.contains("+server.")),
            "{warnings:#?}"
        );
    }

    /// The pilot is the descriptor every other module is tested against; a
    /// check that fires on it is a check nobody will read.
    #[test]
    fn the_pilot_earns_no_dropped_flag_warning() {
        let warnings = report(&rust()).warnings;
        assert!(
            !warnings.iter().any(|w| w.contains("leaves out both")),
            "{warnings:#?}"
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
