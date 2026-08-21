//! Minecraft, as one implementation of [`crate::game::Game`].
//!
//! Everything Minecraft-specific in this crate lives under here: which jar to
//! download and from where, what `server.properties` keys mean, how a player
//! name becomes a UUID, what "Done" in a log line signifies, and which ports
//! the gateway carries. Nothing outside this module knows any of it.
//!
//! That boundary is the point. The tunnel does not know a Java server listens
//! on 25565; this module tells it. The state machine does not know what
//! "Done (12.431s)!" means; this module does. A second game is an addition
//! here and a line in [`crate::game::all`], not a change anywhere else.

pub mod account;
pub mod argfile;
pub mod console;
pub mod crossplay;
pub mod hosting;
pub mod jar;
pub mod jvm;
pub mod loader;
pub mod minigame;
pub mod modjar;
pub mod modpack;
pub mod mods;
pub mod ops;
pub mod settings;
pub mod slp;

use crate::game::{BuildContext, ConfigInput, Encoding, FileWrite, Game, LineMeaning, Lookup};
use crate::tunnel::Forward;
use crate::{Error, Result};
use serde_json::Value;

/// Gateway-facing ports. Fixed by what the gateway DNATs to — see
/// [`crate::tunnel`] before touching them.
pub const LISTEN_JAVA: u16 = 25565;
pub const LISTEN_BEDROCK: u16 = 19132;
pub const LISTEN_VOICE: u16 = 24454;

/// `server.properties` is latin-1; the JSON files are not.
const PROPERTIES: &str = "server.properties";
const OPS: &str = "ops.json";
const WHITELIST: &str = "whitelist.json";
const BANNED: &str = "banned-players.json";

pub struct Minecraft;

impl Game for Minecraft {
    fn id(&self) -> &'static str {
        "minecraft-java"
    }

    fn classify(&self, line: &str) -> LineMeaning {
        LineMeaning {
            ready: console::is_ready(line),
            joined: console::joined(line).map(str::to_string),
            left: console::left(line).map(str::to_string),
            max_players: console::max_players(line),
        }
    }

    fn config_inputs(&self, env: &Value) -> Vec<ConfigInput> {
        let mut inputs = vec![
            // Read so the merge preserves what the server wrote itself, and
            // latin-1 both ways — a MOTD's `§` does not survive a UTF-8
            // round trip.
            ConfigInput {
                path: PROPERTIES.into(),
                encoding: Encoding::Latin1,
            },
            // Read because bans are appended rather than replaced: an
            // in-game `/ban` lands here without any sync seeing it.
            ConfigInput {
                path: BANNED.into(),
                encoding: Encoding::Utf8,
            },
        ];

        // Read only to find out whether it is there: a minigame writes its own
        // `spigot.yml` into a fresh directory and never over Paper's. Asking
        // for it unconditionally would make every ordinary launch read a file
        // it has no intention of touching.
        if minigame::is_minigame(env) {
            inputs.push(ConfigInput {
                path: minigame::SPIGOT_YML.into(),
                encoding: Encoding::Utf8,
            });
        }

        inputs
    }

    fn required_lookups(&self, env: &Value, game_type: &str) -> Vec<Lookup> {
        let resolved = settings::from_env(env, game_type, loader_of(env), None);

        // An offline server's UUIDs are an MD5 of the name, so there is
        // nothing to ask anyone. Returning names here would make the host
        // issue Mojang requests whose answers it must then throw away — and
        // worse, would be wrong: an online UUID does not match an offline
        // server's idea of the same player.
        if !resolved.online_mode {
            return Vec::new();
        }

        let mut names: Vec<String> = Vec::new();
        for list in [
            &resolved.op_users,
            &resolved.whitelisted_users,
            &resolved.banned_users,
        ] {
            for name in list {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }

        names.into_iter().map(|name| Lookup { name }).collect()
    }

    fn config_files(&self, ctx: &BuildContext) -> Result<Vec<FileWrite>> {
        let resolved = settings::from_env(&ctx.env, &ctx.game_type, loader_of(&ctx.env), None);

        let runtime = settings::Runtime {
            port: ctx.port,
            bind_address: ctx.bind_address.clone(),
            // No RCON: hosts using this path drive the console over the
            // process's stdin, and a password for a port nothing listens on is
            // noise. A host that wants RCON calls `settings::properties`
            // directly with one.
            rcon: None,
        };

        let mut files = vec![FileWrite {
            path: PROPERTIES.into(),
            contents: crate::properties::merge(
                ctx.existing(PROPERTIES),
                &settings::properties(&resolved, &runtime),
            ),
            encoding: Encoding::Latin1,
        }];

        // See `settings::resolve_players` for the rule. It lives there rather
        // than here because a host driving a linked engine needs the same
        // answer without producing a file.
        let online = resolved.online_mode;
        let players = |names: &[String]| -> Vec<settings::Player> {
            settings::resolve_players(names, &ctx.resolved, online)
        };

        files.push(FileWrite {
            path: OPS.into(),
            contents: settings::ops_json(&players(&resolved.op_users)).to_string(),
            encoding: Encoding::Utf8,
        });
        files.push(FileWrite {
            path: WHITELIST.into(),
            contents: settings::whitelist_json(&players(&resolved.whitelisted_users)).to_string(),
            encoding: Encoding::Utf8,
        });

        // Boot-time config, so it must be on disk before the JVM reads it —
        // the plugin cannot disable an advancement that has already been
        // granted. See `minigame::disable_advancements` for why this only ever
        // writes into a directory that has no `spigot.yml` yet.
        if let Some(contents) =
            minigame::disable_advancements(&ctx.env, ctx.existing(minigame::SPIGOT_YML))
        {
            files.push(FileWrite {
                path: minigame::SPIGOT_YML.into(),
                contents,
                encoding: Encoding::Utf8,
            });
        }

        // Append-only, and only when there is something new to add.
        let existing_bans = ctx.existing(BANNED);
        let missing = settings::banned_missing(existing_bans, &resolved.banned_users);
        if !missing.is_empty() {
            if let Some(contents) =
                settings::merge_banned(existing_bans, &players(&missing), &ctx.now)
            {
                files.push(FileWrite {
                    path: BANNED.into(),
                    contents,
                    encoding: Encoding::Utf8,
                });
            }
        }

        Ok(files)
    }

    fn forwards(&self, exposure: &str, port: u16, extra: &Value) -> Result<Vec<Forward>> {
        let extra_port = |key: &str, fallback: u16| {
            extra
                .get(key)
                .and_then(|v| v.as_u64())
                .map(|v| v as u16)
                .unwrap_or(fallback)
        };

        let mut forwards = match exposure {
            "java" => vec![Forward::tcp(LISTEN_JAVA, port)],
            "bedrock" => vec![Forward::udp(LISTEN_BEDROCK, port)],
            "crossplay" => vec![
                Forward::tcp(LISTEN_JAVA, port),
                Forward::udp(LISTEN_BEDROCK, extra_port("geyserPort", LISTEN_BEDROCK)),
            ],
            other => {
                return Err(Error::Unsupported(format!(
                    "\"{other}\" is not a way this server can be exposed"
                )))
            }
        };

        if let Some(voice) = extra.get("voiceChatPort").and_then(|v| v.as_u64()) {
            forwards.push(Forward::udp(LISTEN_VOICE, voice as u16));
        }

        Ok(forwards)
    }
}

/// How a server of this game type has to be exposed through the tunnel.
///
/// The answer [`Minecraft::forwards`] expects, derived from the one field that
/// can decide it. A host that inferred this from `TYPE` instead would get
/// crossplay wrong every time — a crossplay server's `TYPE` is an ordinary
/// loader, and the only thing that says Bedrock players are coming is the game
/// type.
///
/// Unknown types answer `java`, which is what every game type that is not one
/// of the two special cases has always been.
pub fn exposure_for(game_type: &str) -> &'static str {
    if hosting::is_bedrock(game_type) {
        "bedrock"
    } else if crossplay::is_crossplay(game_type) {
        "crossplay"
    } else {
        "java"
    }
}

/// The loader named in the environment, normalised the way `TYPE` is.
///
/// Public because a host that configures a **linked** engine needs the same
/// answer and must not re-derive it: the loader is half of the online-mode
/// rule, and a second copy of "TYPE absent or vanilla means vanilla" is how a
/// crossplay server ends up authenticating when it should not.
pub fn loader_of(env: &Value) -> &'static str {
    match env.get("TYPE").and_then(|v| v.as_str()) {
        Some(t) if t.eq_ignore_ascii_case("vanilla") || t.is_empty() => "vanilla",
        None => "vanilla",
        Some(_) => "modded",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::Identity;
    use serde_json::json;

    fn ctx(env: Value) -> BuildContext {
        BuildContext {
            env,
            game_type: "java".into(),
            port: 25565,
            bind_address: "127.0.0.1".into(),
            existing: Default::default(),
            resolved: vec![],
            now: "2026-08-10 14:03:22 +0000".into(),
        }
    }

    #[test]
    fn it_is_registered_under_a_stable_id() {
        assert_eq!(Minecraft.id(), "minecraft-java");
        assert!(crate::game::by_id("minecraft-java").is_some());
    }

    // ─── the capability surface ─────────────────────────────────────────────

    #[test]
    fn a_ready_line_is_classified_through_the_trait() {
        let meaning = Minecraft
            .classify("[12:00:00] [Server thread/INFO]: Done (12.431s)! For help, type \"help\"");
        assert!(meaning.ready);
        assert_eq!(meaning.joined, None);
    }

    #[test]
    fn config_inputs_name_the_files_the_build_reads() {
        let inputs = Minecraft.config_inputs(&json!({}));
        let paths: Vec<&str> = inputs.iter().map(|i| i.path.as_str()).collect();
        assert!(paths.contains(&PROPERTIES));
        assert!(
            paths.contains(&BANNED),
            "bans are appended, so the existing file must be read"
        );
    }

    /// Reading is as easy to get wrong as writing: decoding latin-1 as UTF-8
    /// destroys a MOTD's colour codes on a launch that changed nothing.
    #[test]
    fn an_input_is_read_with_the_same_encoding_it_is_written() {
        let inputs = Minecraft.config_inputs(&json!({}));
        let files = Minecraft.config_files(&ctx(json!({}))).unwrap();

        for input in &inputs {
            let Some(written) = files.iter().find(|f| f.path == input.path) else {
                continue;
            };
            assert_eq!(
                input.encoding, written.encoding,
                "{} is read and written differently",
                input.path
            );
        }
    }

    #[test]
    fn lookups_cover_every_list_without_repeating_a_name() {
        let lookups = Minecraft.required_lookups(
            &json!({
                "OPS": "Notch,Steve",
                "WHITELIST": "Steve,Alex",
                "BANNED": "Griefer",
            }),
            "java",
        );
        let names: Vec<&str> = lookups.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, vec!["Notch", "Steve", "Alex", "Griefer"]);
    }

    /// An offline server derives its own UUIDs, so it must ask for nothing —
    /// a Mojang lookup here would be both wasted and wrong, since an online
    /// UUID does not match an offline server's idea of the same player.
    #[test]
    fn an_offline_server_asks_for_no_lookups() {
        let lookups =
            Minecraft.required_lookups(&json!({ "OPS": "Notch", "ONLINE_MODE": "false" }), "java");
        assert!(lookups.is_empty());
    }

    /// And it still writes the op, with the derived UUID.
    #[test]
    fn an_offline_server_derives_its_ops_without_the_host() {
        let mut context = ctx(json!({ "OPS": "Notch", "ONLINE_MODE": "false" }));
        context.resolved = vec![]; // the host resolved nothing, as instructed

        let files = Minecraft.config_files(&context).unwrap();
        let ops = files.iter().find(|f| f.path == OPS).unwrap();
        assert!(
            ops.contents
                .contains("b50ad385-829d-3141-a216-7e7d7539ba7f"),
            "the offline UUID must be derived in the core: {}",
            ops.contents
        );
    }

    /// Crossplay reaches the same conclusion through a different route, and
    /// the two entry points must agree or the host resolves the wrong thing.
    #[test]
    fn crossplay_vanilla_also_asks_for_nothing() {
        let lookups = Minecraft.required_lookups(&json!({ "OPS": "Notch" }), "native-crossplay");
        assert!(lookups.is_empty(), "crossplay vanilla runs offline");
    }

    // ─── config files ───────────────────────────────────────────────────────

    #[test]
    fn a_build_writes_properties_ops_and_whitelist() {
        let files = Minecraft.config_files(&ctx(json!({}))).unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec![PROPERTIES, OPS, WHITELIST]);
    }

    /// The encoding travels with the file so no host has to know this.
    #[test]
    fn properties_are_latin1_and_the_json_files_are_not() {
        let files = Minecraft.config_files(&ctx(json!({}))).unwrap();
        let by_path = |p: &str| files.iter().find(|f| f.path == p).unwrap();
        assert_eq!(by_path(PROPERTIES).encoding, Encoding::Latin1);
        assert_eq!(by_path(OPS).encoding, Encoding::Utf8);
    }

    #[test]
    fn the_motd_reaches_the_properties_file() {
        let files = Minecraft
            .config_files(&ctx(json!({ "MOTD": "Android tunnel test" })))
            .unwrap();
        let props = files.iter().find(|f| f.path == PROPERTIES).unwrap();
        assert!(props.contents.contains("motd=Android tunnel test"));
    }

    #[test]
    fn an_unresolved_player_is_skipped_rather_than_written() {
        let mut context = ctx(json!({ "OPS": "Notch,Ghost" }));
        context.resolved = vec![Identity {
            name: "Notch".into(),
            id: "b50ad385-829d-3141-a216-7e7d7539ba7f".into(),
        }];

        let files = Minecraft.config_files(&context).unwrap();
        let ops = files.iter().find(|f| f.path == OPS).unwrap();
        assert!(ops.contents.contains("Notch"));
        assert!(
            !ops.contents.contains("Ghost"),
            "an unresolved name must not be written with a bogus uuid"
        );
    }

    #[test]
    fn existing_properties_are_merged_not_replaced() {
        let mut context = ctx(json!({}));
        context
            .existing
            .insert(PROPERTIES.into(), "rate-limit=7\n".into());

        let files = Minecraft.config_files(&context).unwrap();
        let props = files.iter().find(|f| f.path == PROPERTIES).unwrap();
        assert!(props.contents.contains("rate-limit=7"));
        assert!(props.contents.contains("difficulty=normal"));
    }

    #[test]
    fn bans_are_only_written_when_there_is_something_new() {
        let no_bans = Minecraft.config_files(&ctx(json!({}))).unwrap();
        assert!(!no_bans.iter().any(|f| f.path == BANNED));

        let mut context = ctx(json!({ "BANNED": "Griefer" }));
        context.resolved = vec![Identity {
            name: "Griefer".into(),
            id: "uuid".into(),
        }];
        let with = Minecraft.config_files(&context).unwrap();
        assert!(with.iter().any(|f| f.path == BANNED));
    }

    #[test]
    fn a_ban_already_on_disk_is_not_rewritten() {
        let mut context = ctx(json!({ "BANNED": "Griefer" }));
        context
            .existing
            .insert(BANNED.into(), r#"[{"uuid":"x","name":"Griefer"}]"#.into());
        let files = Minecraft.config_files(&context).unwrap();
        assert!(!files.iter().any(|f| f.path == BANNED));
    }

    /// No path a game returns may escape the server's own directory.
    #[test]
    fn written_paths_stay_inside_the_server_directory() {
        let files = Minecraft.config_files(&ctx(json!({}))).unwrap();
        for file in files {
            assert!(!file.path.starts_with('/'), "{} is absolute", file.path);
            assert!(!file.path.contains(".."), "{} traverses", file.path);
        }
    }

    // ─── forwards ───────────────────────────────────────────────────────────

    #[test]
    fn java_is_one_tcp_forward_on_the_gateways_port() {
        let forwards = Minecraft.forwards("java", 25570, &json!({})).unwrap();
        assert_eq!(forwards, vec![Forward::tcp(LISTEN_JAVA, 25570)]);
    }

    #[test]
    fn bedrock_is_udp() {
        let forwards = Minecraft.forwards("bedrock", 19140, &json!({})).unwrap();
        assert_eq!(forwards, vec![Forward::udp(LISTEN_BEDROCK, 19140)]);
    }

    /// The tunnel's `exposure` comes from the game type and nothing else. A
    /// crossplay server's `TYPE` is an ordinary loader, so a host deriving this
    /// from the loader produces a Java-only tunnel for a server sold as
    /// crossplay — it runs, and no Bedrock player can reach it.
    #[test]
    fn exposure_follows_the_game_type() {
        assert_eq!(exposure_for("native-crossplay"), "crossplay");
        assert_eq!(exposure_for("native-bedrock"), "bedrock");
        assert_eq!(exposure_for("bedrock"), "bedrock");
        assert_eq!(exposure_for("native"), "java");
        assert_eq!(exposure_for("native-pumpkin"), "java");
        assert_eq!(exposure_for(""), "java");
    }

    #[test]
    fn crossplay_is_tcp_then_udp() {
        let forwards = Minecraft
            .forwards("crossplay", 25565, &json!({ "geyserPort": 19133 }))
            .unwrap();
        assert_eq!(
            forwards,
            vec![
                Forward::tcp(LISTEN_JAVA, 25565),
                Forward::udp(LISTEN_BEDROCK, 19133),
            ]
        );
    }

    #[test]
    fn voice_chat_appends_its_own_forward() {
        let forwards = Minecraft
            .forwards("java", 25565, &json!({ "voiceChatPort": 24460 }))
            .unwrap();
        assert_eq!(forwards.len(), 2);
        assert_eq!(forwards[1], Forward::udp(LISTEN_VOICE, 24460));
    }

    /// A typo must not silently produce a Java-only config for a crossplay
    /// server — that server would run, and no Bedrock player could join.
    #[test]
    fn an_unknown_exposure_is_refused_rather_than_defaulted() {
        assert!(Minecraft.forwards("crossplaY", 25565, &json!({})).is_err());
        assert!(Minecraft.forwards("", 25565, &json!({})).is_err());
    }

    /// End to end through the generic layer: the game names the ports, the
    /// tunnel renders them, and neither knows about the other's concerns.
    #[test]
    fn forwards_render_through_the_generic_tunnel() {
        let config = crate::tunnel::Config {
            link: crate::tunnel::Link {
                client_privkey: "K".into(),
                gateway_pubkey: "P".into(),
                link_address: "gw:51820".into(),
                address: None,
                allowed_ips: None,
            },
            forwards: Minecraft.forwards("java", 25565, &json!({})).unwrap(),
        };
        let rendered = config.render();
        assert!(rendered.contains("[TCPServerTunnel]"));
        assert!(rendered.contains("ListenPort = 25565"));
    }
}
