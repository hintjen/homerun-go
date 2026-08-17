//! The pack pipeline, driven end to end without a network.

use super::*;
use crate::minecraft::modjar::{Facts, Side};
use serde_json::json;

// ---------------------------------------------------------------------------
// Finding the pack
// ---------------------------------------------------------------------------

#[test]
fn a_version_url_asks_for_that_version() {
    match plan("https://modrinth.com/modpack/cobbleverse/version/abc123") {
        Plan::Ask { url, of } => {
            assert!(url.ends_with("/version/abc123"), "{url}");
            assert_eq!(of, Lookup::Version);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_slug_url_asks_for_the_featured_release_first() {
    match plan("https://modrinth.com/modpack/cobbleverse") {
        Plan::Ask { url, of } => {
            assert!(url.contains("/project/cobbleverse/version"), "{url}");
            assert!(url.contains("version_type=release"), "{url}");
            assert!(url.contains("featured=true"), "{url}");
            assert_eq!(of, Lookup::Latest);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_direct_url_needs_no_request_and_gets_a_stable_name() {
    let first = match plan("https://example.test/packs/mine.mrpack") {
        Plan::Ready { source } => source,
        other => panic!("{other:?}"),
    };
    assert_eq!(first.url, "https://example.test/packs/mine.mrpack");
    assert_eq!(first.cache_key.len(), 16);

    // Stable across calls, and different for a different URL — which is all a
    // cache filename has to be.
    let again = match plan("https://example.test/packs/mine.mrpack") {
        Plan::Ready { source } => source,
        other => panic!("{other:?}"),
    };
    assert_eq!(first.cache_key, again.cache_key);
    let other = match plan("https://example.test/packs/another.mrpack") {
        Plan::Ready { source } => source,
        o => panic!("{o:?}"),
    };
    assert_ne!(first.cache_key, other.cache_key);
}

fn version(id: &str, kind: &str, url: &str) -> serde_json::Value {
    json!({
        "id": id,
        "version_type": kind,
        "files": [{ "primary": true, "url": url, "filename": "pack.mrpack" }],
    })
}

#[test]
fn the_archive_url_and_cache_key_come_from_the_version() {
    let source = source_from(
        Lookup::Version,
        &version("v9", "release", "https://cdn/pack.mrpack"),
    )
    .unwrap()
    .unwrap();
    assert_eq!(source.url, "https://cdn/pack.mrpack");
    assert_eq!(
        source.cache_key, "v9",
        "the version id, so a changed pin is a new file"
    );
}

/// Some packs only ever publish betas.
#[test]
fn a_release_is_preferred_and_a_beta_will_do() {
    let list = json!([version("b1", "beta", "https://cdn/beta.mrpack")]);
    let source = source_from(Lookup::Latest, &list).unwrap().unwrap();
    assert_eq!(source.url, "https://cdn/beta.mrpack");

    let mixed = json!([
        version("b1", "beta", "https://cdn/beta.mrpack"),
        version("r1", "release", "https://cdn/release.mrpack"),
    ]);
    assert_eq!(
        source_from(Lookup::Latest, &mixed).unwrap().unwrap().url,
        "https://cdn/release.mrpack"
    );
}

/// An empty featured-release list is not a failure — it is the signal to ask
/// again without the filter.
#[test]
fn no_featured_release_asks_again_rather_than_failing() {
    assert_eq!(source_from(Lookup::Latest, &json!([])).unwrap(), None);
    assert_eq!(
        fallback_versions_url("https://modrinth.com/modpack/cobbleverse").as_deref(),
        Some("https://api.modrinth.com/v2/project/cobbleverse/version")
    );
}

// ---------------------------------------------------------------------------
// Reading the manifest
// ---------------------------------------------------------------------------

#[test]
fn the_loader_priority_is_neoforge_then_forge_then_quilt_then_fabric() {
    let cases = [
        (
            json!({ "minecraft": "1.21.4", "neoforge": "21.4.157", "forge": "x" }),
            "neoforge",
            "21.4.157",
        ),
        (
            json!({ "minecraft": "1.20.1", "forge": "47.2.17" }),
            "forge",
            "47.2.17",
        ),
        (
            json!({ "minecraft": "1.20.1", "quilt-loader": "0.26" }),
            "quilt",
            "0.26",
        ),
        (
            json!({ "minecraft": "1.21.4", "fabric-loader": "0.16.9" }),
            "fabric",
            "0.16.9",
        ),
    ];
    for (deps, loader, pinned) in cases {
        let out = requires(&json!({ "dependencies": deps })).unwrap();
        assert_eq!(out.loader, loader);
        assert_eq!(out.loader_version.as_deref(), Some(pinned));
    }
}

#[test]
fn a_pack_naming_no_loader_is_fabric_and_one_naming_no_minecraft_is_malformed() {
    let out = requires(&json!({ "dependencies": { "minecraft": "1.21.4" } })).unwrap();
    assert_eq!(out.loader, "fabric");
    assert_eq!(out.loader_version, None);

    assert!(requires(&json!({ "dependencies": {} })).is_err());
    assert!(requires(&json!({})).is_err());
}

// ---------------------------------------------------------------------------
// Deciding what to install
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Host {
    json: Vec<(String, serde_json::Value)>,
    broken: Vec<String>,
    requested: Vec<String>,
}

impl Host {
    fn reply(&mut self, step: &Step) -> Reply {
        let Step::Json { id, url } = step else {
            panic!("a pack never downloads through a step")
        };
        self.requested.push(url.clone());
        if self.broken.iter().any(|b| url.contains(b)) {
            return Reply {
                id: id.clone(),
                error: Some("boom".into()),
                ..Default::default()
            };
        }
        let body = self
            .json
            .iter()
            .find(|(pattern, _)| url.contains(pattern.as_str()))
            .map(|(_, v)| v.clone());
        Reply {
            id: id.clone(),
            json: body,
            error: None,
        }
    }
}

fn drive(inputs: Inputs, host: &mut Host) -> Outcome {
    let mut progress = begin(inputs);
    for _ in 0..16 {
        match progress {
            Progress::Done { outcome } => return outcome,
            Progress::Steps { steps, state } => {
                assert!(!steps.is_empty(), "an empty batch would spin");
                let replies = steps.iter().map(|s| host.reply(s)).collect();
                progress = advance(state, replies);
            }
        }
    }
    panic!("the pack driver never finished")
}

fn manifest_mod(name: &str, sha: &str) -> PackFile {
    PackFile {
        filename: name.into(),
        sha512: Some(sha.into()),
        url: Some(format!("https://cdn/{name}")),
        url_project_id: None,
        facts: None,
    }
}

fn inputs(manifest: Vec<PackFile>, overrides: Vec<PackFile>) -> Inputs {
    Inputs {
        manifest,
        overrides,
        exclude_files: String::new(),
        overrides_exclusions: String::new(),
    }
}

#[test]
fn a_pack_with_only_server_mods_installs_all_of_them() {
    let mut host = Host {
        json: vec![
            (
                "/version_files".into(),
                json!({ "h1": { "project_id": "p1", "dependencies": [] } }),
            ),
            (
                "/projects?ids=".into(),
                json!([{ "id": "p1", "server_side": "required" }]),
            ),
        ],
        ..Default::default()
    };

    let out = drive(
        inputs(vec![manifest_mod("lithium.jar", "h1")], vec![]),
        &mut host,
    );

    assert_eq!(out.download.len(), 1);
    assert_eq!(out.files, vec!["lithium.jar"]);
    assert_eq!(out.projects, vec!["p1"]);
    assert!(out.remove.is_empty());
}

#[test]
fn a_client_only_manifest_mod_is_excluded_and_swept() {
    let mut host = Host {
        json: vec![
            (
                "/version_files".into(),
                json!({ "h1": { "project_id": "sodium", "dependencies": [] } }),
            ),
            (
                "/projects?ids=".into(),
                json!([{ "id": "sodium", "server_side": "unsupported" }]),
            ),
        ],
        ..Default::default()
    };

    let out = drive(
        inputs(vec![manifest_mod("sodium.jar", "h1")], vec![]),
        &mut host,
    );

    assert!(out.download.is_empty());
    assert!(out.files.is_empty());
    assert_eq!(out.remove, vec!["sodium.jar"]);
    assert!(out.projects.is_empty());
}

/// The case a naive exclusion breaks. `chipped` is server-supported and hard
/// depends on `athena`, which Modrinth marks unsupported — dropping it fails
/// loader resolution with "requires athena, which is missing!".
#[test]
fn a_client_only_library_a_kept_mod_requires_survives() {
    let mut host = Host {
        json: vec![
            (
                "/version_files".into(),
                json!({
                    "h1": { "project_id": "chipped", "dependencies": [
                        { "dependency_type": "required", "project_id": "athena" }
                    ]},
                    "h2": { "project_id": "athena", "dependencies": [] },
                    "h3": { "project_id": "iris", "dependencies": [] },
                }),
            ),
            (
                "/projects?ids=".into(),
                json!([
                    { "id": "chipped", "server_side": "required" },
                    { "id": "athena", "server_side": "unsupported" },
                    { "id": "iris", "server_side": "unsupported" },
                ]),
            ),
        ],
        ..Default::default()
    };

    let out = drive(
        inputs(
            vec![
                manifest_mod("chipped.jar", "h1"),
                manifest_mod("athena.jar", "h2"),
                manifest_mod("iris.jar", "h3"),
            ],
            vec![],
        ),
        &mut host,
    );

    assert_eq!(out.files, vec!["chipped.jar", "athena.jar"]);
    assert_eq!(out.remove, vec!["iris.jar"], "only the one nothing needs");
    assert!(
        out.notes.iter().any(|n| n.contains("keeping 1")),
        "{:?}",
        out.notes
    );
}

/// Without the dependency graph, dropping a client-only mod could strip a hard
/// dependency of one being kept, so a failed lookup installs the pack as-is.
#[test]
fn a_failed_version_lookup_installs_the_pack_exactly_as_shipped() {
    let mut host = Host {
        broken: vec!["/version_files".into()],
        ..Default::default()
    };
    let out = drive(
        inputs(vec![manifest_mod("anything.jar", "h1")], vec![]),
        &mut host,
    );

    assert_eq!(out.files, vec!["anything.jar"]);
    assert!(out.remove.is_empty());
    assert!(
        !host.requested.iter().any(|u| u.contains("/projects?ids=")),
        "and does not go on to ask about sides"
    );
}

// ---------------------------------------------------------------------------
// Override jars
// ---------------------------------------------------------------------------

fn override_jar(name: &str, side: Side, mod_id: &str) -> PackFile {
    PackFile {
        filename: name.into(),
        sha512: Some(format!("sha-{name}")),
        url: None,
        url_project_id: None,
        facts: Some(Facts {
            side,
            mod_id: Some(mod_id.into()),
            deps: Vec::new(),
        }),
    }
}

/// A jar that declares itself client-only is excluded without asking anyone.
#[test]
fn an_override_declaring_itself_client_only_is_dropped_without_a_search() {
    let mut host = Host {
        json: vec![("/version_files".into(), json!({}))],
        ..Default::default()
    };
    let out = drive(
        inputs(
            vec![],
            vec![override_jar("citresewn.jar", Side::Client, "citresewn")],
        ),
        &mut host,
    );

    assert_eq!(out.skip_overrides, vec!["citresewn.jar"]);
    assert!(
        !host.requested.iter().any(|u| u.contains("/search")),
        "nothing to ask: the jar already said so"
    );
}

/// `side = "BOTH"` is a weak signal authors leave on client-only mods, so the
/// name search runs anyway — RyoamicLights ships BOTH and crashes a server.
#[test]
fn an_override_declaring_both_is_still_checked_by_name() {
    let mut host = Host {
        json: vec![
            ("/version_files".into(), json!({})),
            (
                "/search".into(),
                json!({ "hits": [{
                    "project_type": "mod",
                    "slug": "ryoamiclights",
                    "server_side": "unsupported",
                }]}),
            ),
        ],
        ..Default::default()
    };

    let out = drive(
        inputs(
            vec![],
            vec![override_jar(
                "RyoamicLights.jar",
                Side::Serverable,
                "ryoamiclights",
            )],
        ),
        &mut host,
    );

    assert_eq!(out.skip_overrides, vec!["RyoamicLights.jar"]);
}

/// The guards that stop the search producing false positives: the hit must be
/// a mod, and its slug must normalise-equal the mod id.
#[test]
fn a_search_hit_that_is_not_an_exact_mod_match_is_ignored() {
    for hit in [
        json!({ "project_type": "shader", "slug": "taa", "server_side": "unsupported" }),
        json!({ "project_type": "mod", "slug": "modern-ui", "server_side": "unsupported" }),
    ] {
        let mut host = Host {
            json: vec![
                ("/version_files".into(), json!({})),
                ("/search".into(), json!({ "hits": [hit] })),
            ],
            ..Default::default()
        };
        let out = drive(
            inputs(
                vec![],
                vec![override_jar("taa.jar", Side::Serverable, "taa")],
            ),
            &mut host,
        );
        assert!(out.skip_overrides.is_empty(), "{:?}", out.skip_overrides);
        assert_eq!(out.files, vec!["taa.jar"]);
    }
}

/// modId and slug need not match exactly: `shouldersurfing` is published as
/// `shoulder-surfing-reloaded`... but normalisation only strips punctuation,
/// so this one must NOT match — the guard is deliberately strict.
#[test]
fn normalisation_strips_punctuation_and_case_only() {
    assert_eq!(normalise("Shoulder-Surfing"), "shouldersurfing");
    assert_ne!(
        normalise("shoulder-surfing-reloaded"),
        normalise("shouldersurfing")
    );
}

// ---------------------------------------------------------------------------
// The manual escape hatches
// ---------------------------------------------------------------------------

#[test]
fn exclude_files_matches_a_partial_filename() {
    let patterns = vec!["rubidium-extra".to_string()];
    assert!(matches_exclude("rubidium-extra-0.4.18.jar", &patterns));
    assert!(!matches_exclude("rubidium-0.4.18.jar", &patterns));
}

#[test]
fn ant_globs_match_the_way_ant_globs_do() {
    assert!(ant_matches(
        "mods/torohealth-*.jar",
        "mods/torohealth-1.2.jar"
    ));
    assert!(
        !ant_matches("mods/*.jar", "mods/nested/x.jar"),
        "* stays in a segment"
    );
    assert!(
        ant_matches("mods/**/*.jar", "mods/nested/x.jar"),
        "** crosses them"
    );
    assert!(ant_matches("mods/????.jar", "mods/abcd.jar"));
    assert!(!ant_matches("mods/????.jar", "mods/abc.jar"));
    assert!(
        !ant_matches("mods/x.jar", "other/mods/x.jar"),
        "matched whole"
    );
}

#[test]
fn the_manual_hatches_drop_files_the_automatic_pass_kept() {
    let mut host = Host {
        json: vec![
            (
                "/version_files".into(),
                json!({ "h1": { "project_id": "p1", "dependencies": [] } }),
            ),
            (
                "/projects?ids=".into(),
                json!([{ "id": "p1", "server_side": "required" }]),
            ),
        ],
        ..Default::default()
    };

    let mut input = inputs(
        vec![manifest_mod("rubidium-extra-0.4.18.jar", "h1")],
        vec![override_jar(
            "torohealth-1.2.jar",
            Side::Serverable,
            "torohealth",
        )],
    );
    input.exclude_files = "rubidium-extra".into();
    input.overrides_exclusions = "mods/torohealth-*.jar".into();

    let out = drive(input, &mut host);

    assert!(out.download.is_empty());
    assert!(out.files.is_empty());
    assert_eq!(out.skip_overrides, vec!["torohealth-1.2.jar"]);
    assert!(out
        .notes
        .iter()
        .any(|n| n.contains("MODRINTH_EXCLUDE_FILES")));
    assert!(out
        .notes
        .iter()
        .any(|n| n.contains("MODRINTH_OVERRIDES_EXCLUSIONS")));
}

// ---------------------------------------------------------------------------
// Reconciliation
// ---------------------------------------------------------------------------

fn assembled(name: &str, mod_id: &str, deps: &[&str], client_only: bool) -> Assembled {
    Assembled {
        filename: name.into(),
        facts: Facts {
            side: Side::Unknown,
            mod_id: Some(mod_id.into()),
            deps: deps.iter().map(|d| d.to_string()).collect(),
        },
        client_only,
    }
}

/// Modrinth said sodiumoptionsapi needs only Sodium; its own toml still
/// hard-requires reeses_sodium_options, which was excluded. NeoForge enforces
/// the jar, so the rescued mod has to go too.
#[test]
fn a_rescued_client_only_mod_with_a_missing_hard_dependency_is_pruned() {
    let jars = vec![
        assembled("sodium.jar", "sodium", &[], false),
        assembled(
            "sodiumoptionsapi.jar",
            "sodiumoptionsapi",
            &["sodium", "reeses_sodium_options"],
            true,
        ),
    ];
    assert_eq!(reconcile(&jars), vec!["sodiumoptionsapi.jar"]);
}

/// A server-installable mod missing a dependency is a real modpack error and
/// is reported by the loader, not silently papered over here.
#[test]
fn a_server_mod_with_a_missing_dependency_is_left_alone() {
    let jars = vec![assembled("chipped.jar", "chipped", &["athena"], false)];
    assert!(reconcile(&jars).is_empty());
}

#[test]
fn pruning_cascades_to_whatever_depended_on_what_was_pruned() {
    let jars = vec![
        assembled("base.jar", "base", &["gone"], true),
        assembled("leaf.jar", "leaf", &["base"], true),
    ];
    let pruned = reconcile(&jars);
    assert!(pruned.contains(&"base.jar".to_string()), "{pruned:?}");
    assert!(pruned.contains(&"leaf.jar".to_string()), "{pruned:?}");
}

#[test]
fn a_satisfied_client_only_mod_stays() {
    let jars = vec![
        assembled("athena.jar", "athena", &[], true),
        assembled("chipped.jar", "chipped", &["athena"], false),
    ];
    assert!(reconcile(&jars).is_empty());
}
