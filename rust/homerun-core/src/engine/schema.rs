//! The descriptor's JSON Schema, exported from the Rust types.
//!
//! # Which one is the source of truth
//!
//! The Rust types are. This module writes them out as a schema so that
//! editors, the onboarding skill and the monorepo's CI have something to
//! check a `games/<slug>/game.json` against without a Rust toolchain; the
//! monorepo pins a copy at `games/schema/game.v0.json` and checks the two
//! agree.
//!
//! # Why it is written by hand
//!
//! `homerun-core` has three dependencies and a standing argument in
//! `Cargo.toml` for why each one earns its place. `schemars` would be a
//! fourth, pulled in to generate a document that changes a few times a year.
//!
//! The obvious risk of hand-writing it is drift — a field added to the types
//! and forgotten here. That is what
//! [`tests::the_schema_names_every_field_the_types_serialise`] is for: it
//! serialises a descriptor with **every** field populated and asserts the
//! schema describes each one. Adding a field to the types without adding it
//! here fails that test, by name.
//!
//! # The generated file is committed
//!
//! `schema/game.v0.json` at the crate root is the output of [`document`],
//! checked in. [`tests::the_committed_schema_is_not_stale`] fails when the two
//! disagree, and says which command regenerates it.
//!
//! Committing a generated file usually earns its keep only if something
//! cannot generate it, and here two things cannot: the monorepo pins a copy at
//! `games/schema/game.v0.json` and wants to diff it in CI without a Rust
//! toolchain, and a reviewer wants to see a schema change *as a diff* rather
//! than infer it from a change to a `json!` macro.
//!
//! # What the schema does *not* do
//!
//! It describes shape, not sense. Everything in [`super::validate`] — a
//! default outside its own bounds, a port an argument names but the
//! descriptor does not declare, a secret in the join URL — is beyond what
//! JSON Schema can say, and a descriptor that validates against this document
//! can still be refused. The schema is an editor's autocomplete and a first
//! filter; `validate` is the gate.

use serde_json::{json, Value};

use super::descriptor::SCHEMA_VERSION;

/// The `$id` the monorepo's pinned copy is published under.
pub const SCHEMA_ID: &str = "https://gethomerun.app/schemas/game.v0.json";

/// The descriptor schema, as a JSON Schema 2020-12 document.
pub fn schema() -> Value {
    let mut document = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": SCHEMA_ID,
        "title": "Homerun game descriptor",
        "description":
            "One hostable game. Generated from homerun-core::engine::descriptor -- \
             those types are the source of truth. Unknown properties are allowed on \
             purpose: a descriptor written for a newer Homerun must degrade rather \
             than fail.",
        "type": "object",
        "required": ["id", "name", "hosts"],
        "properties": {
            "schema": {
                "type": "integer",
                "maximum": SCHEMA_VERSION,
                "description": "Schema version. A higher number is refused."
            },
            "id": {
                "type": "string",
                "pattern": "^[a-z0-9-]+$",
                "description": "Slug, and the games/<id>/ directory name."
            },
            "name": { "type": "string" },
            "hosts": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Declared, never inferred. Every entry needs a matching `platforms` key."
            },

            "catalog": {
                "type": "object",
                "properties": {
                    "blurb": { "type": "string" },
                    "accent": { "type": "string", "pattern": "^#[0-9a-fA-F]{6}$" },
                    "art": { "type": "string" },
                    "listed": {
                        "type": ["boolean", "null"],
                        "description":
                            "false when Homerun no longer offers new servers of this \
                             game. Existing servers keep working. Absent means listed. \
                             The runner ignores it."
                    }
                }
            },
            "licence": {
                "type": ["object", "null"],
                "required": ["name", "url"],
                "properties": {
                    "name": { "type": "string" },
                    "url": { "type": "string" },
                    "documents": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["name", "url"],
                            "properties": {
                                "name": { "type": "string" },
                                "url": { "type": "string" }
                            }
                        }
                    },
                    "acceptVia": {
                        "type": ["object", "null"],
                        "required": ["file", "contents"],
                        "properties": {
                            "file": { "type": "string" },
                            "contents": { "type": "string" }
                        }
                    }
                }
            },
            "client": {
                "type": "object",
                "properties": {
                    "joinUrl": { "type": ["string", "null"] },
                    "srv": {
                        "type": ["string", "null"],
                        "description": "Set only when the game's client resolves SRV records."
                    }
                }
            },
            "settings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["key", "type"],
                    "properties": {
                        "key": { "type": "string" },
                        "type": { "enum": ["string", "int", "bool"] },
                        "label": { "type": "string" },
                        "default": {
                            "description":
                                "May be null, which drops the setting's flag from the \
                                 launch line. A string default may contain {serverName} \
                                 and no other placeholder."
                        },
                        "min": {
                            "type": ["integer", "null"],
                            "description":
                                "Inclusive, and only meaningful for an int setting. \
                                 Ignored on any other type."
                        },
                        "max": {
                            "type": ["integer", "null"],
                            "description":
                                "Inclusive, and only meaningful for an int setting. \
                                 Ignored on any other type."
                        },
                        "options": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description":
                                "A closed set of choices, for a string setting only. \
                                 A non-empty list is the only way to say \"pick one of \
                                 these\" -- there is no enum type. An empty list means \
                                 the same as no list."
                        },
                        "optionLabels": {
                            "type": "object",
                            "additionalProperties": { "type": "string" },
                            "description":
                                "What each option is called on screen, keyed by the \
                                 option. The option is still what is stored and sent. \
                                 For the UI; the runner ignores it."
                        },
                        "createOnly": {
                            "type": "boolean",
                            "description":
                                "Chosen once, when the server is created: the game \
                                 reads it only when its world is made. The API refuses \
                                 a change afterwards. The runner ignores it."
                        },
                        "showWhen": {
                            "type": "object",
                            "additionalProperties": {
                                "type": "array",
                                "items": { "type": "string" }
                            },
                            "description":
                                "Show the setting only while every named setting holds \
                                 one of the listed values. Display only; one level \
                                 deep. The runner ignores it."
                        },
                        "group": {
                            "type": ["string", "null"],
                            "description":
                                "A heading to show the setting under. The runner \
                                 ignores it."
                        },
                        "secret": {
                            "type": "boolean",
                            "description":
                                "A value the UI masks, such as a server password a \
                                 player chooses. Not a host-generated {secret:<name>}. \
                                 The runner ignores it."
                        },
                        "optionsFrom": {
                            "type": ["string", "null"],
                            "description":
                                "Where the choices are read from at run time instead of \
                                 options. Only \"versions\" today: the game's version \
                                 list, for a vendor runtime's versionSetting. The runner \
                                 ignores it."
                        }
                    }
                }
            },

            "requires": {
                "type": "object",
                "properties": {
                    "ramMb": { "type": "integer", "minimum": 0 },
                    "diskMb": { "type": "integer", "minimum": 0 },
                    "cpuCores": { "type": ["integer", "null"], "minimum": 0 }
                }
            },
            "platforms": {
                "type": "object",
                "description": "Keyed by host platform, e.g. win32-x64.",
                "additionalProperties": {
                    "type": "object",
                    "properties": {
                        "runtime": {
                            "type": "object",
                            "properties": {
                                "source": { "enum": ["direct", "steamcmd", "vendor"] },
                                "url": {
                                    "type": ["string", "null"],
                                    "description":
                                        "For vendor, an https pattern carrying {version} or \
                                         {versionDigits} (the version without its dots) in \
                                         its path, and no other placeholder."
                                },
                                "sha256": {
                                    "type": ["string", "null"], "pattern": "^[0-9a-f]{64}$",
                                    "description": "Direct only. A vendor download is not pinned by digest."
                                },
                                "size": { "type": ["integer", "null"], "minimum": 0 },
                                "extract": { "enum": ["none", "zip", null] },
                                "stripComponents": {
                                    "type": ["integer", "null"], "minimum": 0,
                                    "description":
                                        "Leading path components dropped from every zip \
                                         entry, for an archive nested under one top folder."
                                },
                                "versionSetting": {
                                    "type": ["string", "null"],
                                    "description":
                                        "Vendor only: the string setting holding the \
                                         player's choice of version. The host resolves it to \
                                         one concrete version; the engine never lists them."
                                },
                                "appId": { "type": ["integer", "null"], "minimum": 0 },
                                "buildId": { "type": ["string", "null"] },
                                "sizeMb": { "type": ["integer", "null"], "minimum": 0 }
                            },
                            "allOf": [
                                {
                                    "if": { "properties": { "source": { "const": "direct" } },
                                            "required": ["source"] },
                                    "then": { "required": ["url", "sha256"] }
                                },
                                {
                                    "if": { "properties": { "source": { "const": "steamcmd" } },
                                            "required": ["source"] },
                                    "then": { "required": ["appId"] }
                                },
                                {
                                    "if": { "properties": { "source": { "const": "vendor" } },
                                            "required": ["source"] },
                                    "then": {
                                        "required": ["url", "extract", "versionSetting"],
                                        "properties": {
                                            "url": { "type": "string", "pattern": "^https://" },
                                            "extract": { "const": "zip" }
                                        }
                                    }
                                }
                            ]
                        },
                        "launch": {
                            "type": "object",
                            "required": ["exe"],
                            "properties": {
                                "exe": {
                                    "type": "string",
                                    "description":
                                        "Relative to the runtime directory, without a \
                                         platform suffix. Never templated."
                                },
                                "args": { "type": "array", "items": { "type": "string" } },
                                "env": {
                                    "type": "object",
                                    "additionalProperties": { "type": "string" }
                                },
                                "cwd": { "type": ["string", "null"] },
                                "cwdBase": { "enum": ["server", "runtime"], "default": "server",
                                    "description": "Directory cwd is relative to. Runtime is shared; keep saves under serverDir using an absolute data path or saves.mounts." }
                            }
                        }
                    }
                }
            },
            "ready": {
                "type": "object",
                "properties": {
                    "marker": {
                        "type": "string",
                        "description": "A substring of one console line. Not a regex."
                    },
                    "timeoutMs": { "type": "integer", "minimum": 0 }
                }
            },
            "console": {
                "type": "object",
                "properties": {
                    "via": { "enum": ["stdin", "rcon", "none"] },
                    "rcon": {
                        "type": ["object", "null"],
                        "properties": {
                            "protocol": { "enum": ["source", "webrcon"] },
                            "port": {
                                "type": "string",
                                "description": "The name of a declared port, not a number."
                            },
                            "secret": {
                                "type": "string",
                                "description": "The name of a generated secret, never a literal."
                            }
                        }
                    }
                }
            },
            "stop": {
                "type": "object",
                "properties": {
                    "via": { "enum": ["console", "interrupt"] },
                    "command": { "type": ["string", "null"] },
                    "graceMs": { "type": "integer", "minimum": 0 }
                }
            },
            "ports": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["name", "port"],
                    "properties": {
                        "name": { "type": "string" },
                        "proto": { "enum": ["tcp", "udp"] },
                        "port": {
                            "type": "integer", "minimum": 1, "maximum": 65535,
                            "description":
                                "What the server binds AND the gateway dest port. Never \
                                 the public port, which the allocator assigns."
                        },
                        "expose": { "type": "boolean" },
                        "service": { "type": ["string", "null"] }
                    }
                }
            },
            "config": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["file"],
                    "properties": {
                        "file": { "type": "string" },
                        "format": { "enum": ["properties", "ini", "json", "toml", "xml"] },
                        "keys": { "type": "object", "additionalProperties": { "type": "string" } }
                    }
                }
            },
            "saves": {
                "type": "object",
                "properties": {
                    "paths": { "type": "array", "items": { "type": "string" } },
                    "excludes": { "type": "array", "items": { "type": "string" } },
                    "mounts": { "type": "array", "items": {
                        "type": "object", "required": ["runtime", "server"],
                        "properties": {
                            "runtime": { "type": "string", "minLength": 1, "description": "Fixed directory path relative to the game's runtime. No traversal, aliases or placeholders." },
                            "server": { "type": "string", "minLength": 1, "description": "Real directory relative to this server's folder; saves physically live here." }
                        }
                    } }
                }
            },
            "observe": {
                "type": "object",
                "properties": {
                    "players": { "enum": ["a2s", "rcon", "log-regex", "none"] },
                    "playersCommand": { "type": ["string", "null"] },
                    "presence": {
                        "type": ["object", "null"],
                        "properties": {
                            "join": { "type": "string" },
                            "leave": { "type": "string" }
                        }
                    },
                    "ping": { "enum": ["a2s", "tcp", "none"] }
                }
            },
            "mods": {
                "type": "object",
                "properties": { "supported": { "type": "boolean" } }
            },
            "limits": {
                "description": "Reserved for per-game ceilings. Nothing reads it yet."
            }
        }
    });
    // Built apart: the document above is already close to `json!`'s
    // recursion limit, and this part is generated from the registry.
    document["properties"]["extension"] = extension(super::extensions::PUBLISHED);
    document
}

/// `extension`, from the extensions a release build carries.
///
/// Generated rather than written, so a new extension cannot leave the schema
/// behind: its name joins the enum, and its own config schema applies when
/// the descriptor names it. The test-only reference extension is never here.
fn extension(published: &[super::extensions::ExtensionSpec]) -> Value {
    let mut name = json!({
        "type": "string",
        "pattern": "^[a-z0-9-]+$",
        "description":
            "An extension compiled into the runner. A runner without it refuses \
             the descriptor."
    });
    if !published.is_empty() {
        name["enum"] = published.iter().map(|s| json!(s.name)).collect();
    }
    let configs: Vec<Value> = published
        .iter()
        .map(|spec| {
            json!({
                "if": { "properties": { "name": { "const": spec.name } }, "required": ["name"] },
                "then": { "properties": { "config": (spec.config_schema)() } }
            })
        })
        .collect();
    let mut extension = json!({
        "type": "object",
        "required": ["name"],
        "description":
            "Code only this game needs, compiled into the runner and chosen by name. \
             Its values reach the launch through {extension:<key>}; a secret one only \
             through launch.env.",
        "properties": {
            "name": name,
            "config": {
                "type": ["object", "null"],
                "description": "Data for the named extension, which validates it."
            }
        }
    });
    if !configs.is_empty() {
        extension["allOf"] = Value::Array(configs);
    }
    extension
}

/// The schema as the file the monorepo pins, newline-terminated.
pub fn document() -> String {
    format!("{}\n", serde_json::to_string_pretty(&schema()).unwrap())
}

/// The committed copy of [`document`], at `schema/game.v0.json`.
///
/// Callers that only want to hand the schema to something — a validator, a
/// pinning script — should use this rather than re-rendering it, so that what
/// they act on is the file that was reviewed.
pub const COMMITTED: &str = include_str!("../../schema/game.v0.json");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::descriptor::*;
    use std::collections::BTreeMap;

    /// A descriptor with **every** field set to something non-default.
    ///
    /// Its only job is to serialise to an object containing every key the
    /// types can produce, so the drift test below has something complete to
    /// compare against. A field added to the types without being added here
    /// fails to compile, because this is built with no `..Default::default()`.
    fn fully_populated() -> GameDescriptor {
        GameDescriptor {
            schema: 0,
            id: "example".into(),
            name: "Example".into(),
            hosts: vec!["win32-x64".into()],
            catalog: Catalog {
                blurb: "b".into(),
                accent: "#CD412B".into(),
                art: "art/card.png".into(),
                listed: Some(false),
            },
            licence: Some(Licence {
                name: "Terms".into(),
                url: "https://example/terms".into(),
                documents: vec![LicenceDocument {
                    name: "Terms".into(),
                    url: "https://example/terms".into(),
                }],
                accept_via: Some(AcceptVia {
                    file: "eula.txt".into(),
                    contents: "eula=true\n".into(),
                }),
            }),
            client: Client {
                join_url: Some("steam://connect/{host}:{port:game}".into()),
                srv: Some("_example".into()),
            },
            settings: vec![Setting {
                key: "maxPlayers".into(),
                kind: SettingKind::Int,
                label: "Max players".into(),
                default: serde_json::json!(10),
                min: Some(1),
                max: Some(200),
                options: vec![serde_json::json!("10")],
                option_labels: BTreeMap::from([("10".to_string(), "Ten".to_string())]),
                create_only: true,
                show_when: BTreeMap::from([("mode".to_string(), vec!["hard".to_string()])]),
                group: Some("Players".into()),
                secret: true,
                options_from: Some("versions".into()),
            }],
            requires: Requires {
                ram_mb: 8192,
                disk_mb: 16000,
                cpu_cores: Some(4),
            },
            platforms: BTreeMap::from([(
                "win32-x64".to_string(),
                Platform {
                    runtime: Runtime {
                        source: RuntimeSource::Direct,
                        url: Some("https://example/s.zip".into()),
                        sha256: Some("a".repeat(64)),
                        size: Some(1),
                        extract: Some(Extract::Zip),
                        strip_components: Some(1),
                        version_setting: Some("version".into()),
                        app_id: Some(258550),
                        build_id: Some("1".into()),
                        size_mb: Some(9000),
                    },
                    launch: Launch {
                        exe: "S.exe".into(),
                        args: vec!["-batchmode".into()],
                        env: BTreeMap::from([("K".to_string(), "v".to_string())]),
                        cwd: Some(".".into()),
                        cwd_base: CwdBase::Runtime,
                    },
                },
            )]),
            ready: Ready {
                marker: "up".into(),
                timeout_ms: 1,
            },
            console: Console {
                via: ConsoleVia::Rcon,
                rcon: Some(Rcon {
                    protocol: RconProtocol::Webrcon,
                    port: "rcon".into(),
                    secret: "rcon".into(),
                }),
            },
            stop: Stop {
                via: StopVia::Console,
                command: Some("quit".into()),
                grace_ms: 1,
            },
            ports: vec![Port {
                name: "game".into(),
                proto: crate::tunnel::Protocol::Udp,
                port: 28015,
                expose: true,
                service: Some("game".into()),
            }],
            config: vec![ConfigFile {
                file: "server.properties".into(),
                format: ConfigFormat::Properties,
                keys: BTreeMap::from([("motd".to_string(), "{serverName}".to_string())]),
            }],
            saves: Saves {
                paths: vec!["world".into()],
                excludes: vec!["*.log".into()],
                mounts: vec![Mount {
                    runtime: "server".into(),
                    server: "server".into(),
                }],
            },
            observe: Observe {
                players: PlayersVia::Rcon,
                players_command: Some("playerlist".into()),
                presence: Some(Presence {
                    join: "joined".into(),
                    leave: "left".into(),
                }),
                ping: PingVia::A2s,
            },
            mods: Mods { supported: true },
            extension: Some(Extension {
                name: "fixture".into(),
                // Empty: its keys are the extension's, and not the schema's.
                config: serde_json::json!({}),
            }),
            limits: serde_json::json!({}),
        }
    }

    /// A published extension joins the name enum, and its own config schema
    /// applies when a descriptor names it -- checked on the reference
    /// extension, since no release extension exists yet.
    #[test]
    fn a_published_extension_brings_its_name_and_config_schema() {
        let generated = extension(&[crate::engine::extensions::fixture::SPEC]);
        assert_eq!(
            generated["properties"]["name"]["enum"],
            serde_json::json!(["fixture"])
        );
        let rule = &generated["allOf"][0];
        assert_eq!(rule["if"]["properties"]["name"]["const"], "fixture");
        assert_eq!(
            rule["then"]["properties"]["config"],
            (crate::engine::extensions::fixture::SPEC.config_schema)()
        );

        let none = extension(&[]);
        assert!(none["properties"]["name"].get("enum").is_none());
        assert!(none.get("allOf").is_none());
    }

    /// Every property name the types can serialise has to appear somewhere in
    /// the schema.
    ///
    /// This is the drift alarm. A field added to `descriptor.rs` and
    /// forgotten here fails this test naming the field, rather than shipping
    /// a schema that silently describes less than the types do.
    #[test]
    fn the_schema_names_every_field_the_types_serialise() {
        let document = schema();
        let rendered = serde_json::to_string(&document).unwrap();

        let mut missing = Vec::new();
        collect_keys(
            &serde_json::to_value(fully_populated()).unwrap(),
            &mut |key| {
                // A key appears in the schema as a property name. Values
                // inside `keys`/`env` maps are caller-chosen and are
                // described by `additionalProperties`, so they are skipped.
                if !rendered.contains(&format!("\"{key}\"")) {
                    missing.push(key.to_string());
                }
            },
        );

        assert!(
            missing.is_empty(),
            "these fields exist in the Rust types and not in the schema: {missing:?}"
        );
    }

    /// Walk an object's property names, skipping the maps whose keys are the
    /// descriptor author's rather than ours.
    fn collect_keys(value: &Value, out: &mut impl FnMut(&str)) {
        // `optionLabels` is keyed by a setting's options and `showWhen` by
        // other settings' keys - the author's names, like the other three.
        const CALLER_KEYED: [&str; 5] = ["env", "keys", "platforms", "optionLabels", "showWhen"];
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    out(key);
                    if CALLER_KEYED.contains(&key.as_str()) {
                        // Descend past the caller's own keys into the shapes
                        // underneath them.
                        if let Value::Object(inner) = child {
                            for grandchild in inner.values() {
                                collect_keys(grandchild, out);
                            }
                        }
                    } else {
                        collect_keys(child, out);
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    collect_keys(item, out);
                }
            }
            _ => {}
        }
    }

    /// The committed file has to be what the types produce today.
    ///
    /// Without this, `schema/game.v0.json` is a copy that was true once — and
    /// the monorepo pins *it*, so a stale copy would be checked against real
    /// descriptors in CI and quietly pass things this build refuses.
    #[test]
    fn the_committed_schema_is_not_stale() {
        assert_eq!(
            COMMITTED,
            document(),
            "\n\nrust/homerun-core/schema/game.v0.json is out of date with the \
             types in engine::descriptor.\nRegenerate it:\n\n    npm run \
             schema:descriptor -- rust/homerun-core/schema/game.v0.json\n\n"
        );
    }

    #[test]
    fn the_schema_is_a_2020_12_document_with_a_stable_id() {
        let s = schema();
        assert_eq!(s["$schema"], "https://json-schema.org/draft/2020-12/schema");
        assert_eq!(s["$id"], SCHEMA_ID);
        assert_eq!(s["type"], "object");
    }

    /// The schema must not forbid what the types deliberately allow, or the
    /// monorepo's CI would reject the descriptors this build happily runs.
    #[test]
    fn the_schema_leaves_room_for_a_newer_descriptor() {
        let s = schema();
        assert!(
            s.get("additionalProperties").is_none(),
            "unknown top-level keys must stay legal -- a newer descriptor has to degrade"
        );
    }

    #[test]
    fn the_schema_pins_the_version_this_build_reads() {
        assert_eq!(schema()["properties"]["schema"]["maximum"], SCHEMA_VERSION);
    }

    #[test]
    fn the_document_is_pretty_printed_and_newline_terminated() {
        let d = document();
        assert!(d.ends_with("}\n"));
        assert!(d.contains("\n  \""), "pretty printed so a diff is readable");
    }

    /// The enums in the schema have to be the enums in the types. Spot-checked
    /// against the spellings the descriptor actually writes, because a schema
    /// that allows `"webRcon"` is one an editor will happily autocomplete into
    /// a descriptor this build refuses.
    #[test]
    fn the_schemas_enums_match_what_serde_writes() {
        let s = schema();
        let console_via = &s["properties"]["console"]["properties"]["via"]["enum"];
        for expected in [ConsoleVia::Stdin, ConsoleVia::Rcon, ConsoleVia::None] {
            let spelled = serde_json::to_value(expected).unwrap();
            assert!(
                console_via.as_array().unwrap().contains(&spelled),
                "schema does not allow {spelled}"
            );
        }

        let players = &s["properties"]["observe"]["properties"]["players"]["enum"];
        for expected in [
            PlayersVia::A2s,
            PlayersVia::Rcon,
            PlayersVia::LogRegex,
            PlayersVia::None,
        ] {
            let spelled = serde_json::to_value(expected).unwrap();
            assert!(
                players.as_array().unwrap().contains(&spelled),
                "schema does not allow {spelled}"
            );
        }
    }
}
