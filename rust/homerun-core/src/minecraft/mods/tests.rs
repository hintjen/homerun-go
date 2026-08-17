//! The pipeline, driven end to end without a network.
//!
//! Every test here runs `begin` → `advance`* → `Done` through a fake host, so
//! what is asserted is the behaviour a server actually gets rather than the
//! shape of an intermediate. `mod-installer.ts` is the reference and the test
//! names say which of its behaviours they pin.

use super::*;
use serde_json::json;

// ---------------------------------------------------------------------------
// A fake host
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Host {
    /// Matched as a substring of the step's URL, first match wins.
    json: Vec<(String, Value)>,
    /// URLs whose request fails.
    broken: Vec<String>,
    /// Filenames whose download fails.
    unfetchable: Vec<String>,
    /// Every download the driver asked for, in order.
    downloaded: Vec<String>,
    /// Every JSON URL the driver asked for, in order.
    requested: Vec<String>,
}

impl Host {
    fn answering(pairs: Vec<(&str, Value)>) -> Self {
        Host {
            json: pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            ..Default::default()
        }
    }

    fn reply(&mut self, step: &Step) -> Reply {
        match step {
            Step::Json { id, url } => {
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
                match body {
                    Some(json) => Reply {
                        id: id.clone(),
                        json: Some(json),
                        error: None,
                    },
                    None => Reply {
                        id: id.clone(),
                        error: Some(format!("no canned answer for {url}")),
                        ..Default::default()
                    },
                }
            }
            Step::Download { id, filename, .. } => {
                self.downloaded.push(filename.clone());
                if self.unfetchable.contains(filename) {
                    Reply {
                        id: id.clone(),
                        error: Some("download failed".into()),
                        ..Default::default()
                    }
                } else {
                    Reply {
                        id: id.clone(),
                        json: None,
                        error: None,
                    }
                }
            }
        }
    }
}

fn drive(inputs: Inputs, host: &mut Host) -> Outcome {
    let mut progress = begin(inputs);
    // A dependency graph is finite and deduplicated, so this only ever trips
    // on a bug that fails to converge — which is exactly what it is for.
    for _ in 0..64 {
        match progress {
            Progress::Done { outcome } => return outcome,
            Progress::Steps { steps, state } => {
                assert!(!steps.is_empty(), "a batch with no steps would spin");
                let replies = steps.iter().map(|s| host.reply(s)).collect();
                progress = advance(state, replies);
            }
        }
    }
    panic!("the driver never reached Done");
}

// ---------------------------------------------------------------------------
// Canned Modrinth
// ---------------------------------------------------------------------------

fn version(project: &str, version_id: &str, filename: &str, kind: &str, deps: &[&str]) -> Value {
    json!({
        "id": version_id,
        "project_id": project,
        "version_type": kind,
        "files": [{ "primary": true, "url": format!("https://cdn/{filename}"), "filename": filename }],
        "dependencies": deps.iter().map(|d| json!({
            "dependency_type": "required", "project_id": d
        })).collect::<Vec<_>>(),
    })
}

fn project(id: &str, server_side: &str) -> Value {
    json!({ "id": id, "server_side": server_side, "client_side": "required" })
}

fn inputs(loader: &str, projects: &str) -> Inputs {
    Inputs {
        loader: loader.into(),
        game_version: "1.21.4".into(),
        projects: projects.into(),
        excluded: String::new(),
        existing: BTreeMap::new(),
        modpack_files: Vec::new(),
        modpack_projects: Vec::new(),
        present: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Where mods go
// ---------------------------------------------------------------------------

#[test]
fn the_bukkit_family_gets_plugins_and_everything_else_gets_mods() {
    for loader in ["paper", "spigot", "bukkit"] {
        assert_eq!(sub_dir(loader), "plugins", "{loader}");
    }
    for loader in ["fabric", "forge", "neoforge", "quilt", "vanilla"] {
        assert_eq!(sub_dir(loader), "mods", "{loader}");
    }
}

/// Spigot and Bukkit plugins are published on Modrinth as Paper's.
#[test]
fn spigot_and_bukkit_resolve_against_papers_facet() {
    assert_eq!(modrinth_facet("spigot"), "paper");
    assert_eq!(modrinth_facet("bukkit"), "paper");
    assert_eq!(modrinth_facet("paper"), "paper");
    assert_eq!(modrinth_facet("fabric"), "fabric");
}

// ---------------------------------------------------------------------------
// The happy path
// ---------------------------------------------------------------------------

#[test]
fn a_listed_mod_is_resolved_downloaded_and_recorded() {
    let mut host = Host::answering(vec![
        (
            "/project/lithium/version",
            json!([version(
                "lith01",
                "v1",
                "lithium-1.21.4.jar",
                "release",
                &[]
            )]),
        ),
        ("/projects?ids=", json!([project("lith01", "required")])),
    ]);

    let outcome = drive(inputs("fabric", "lithium"), &mut host);

    assert_eq!(outcome.installed, vec!["lithium"]);
    assert_eq!(host.downloaded, vec!["lithium-1.21.4.jar"]);
    assert!(outcome.failed.is_empty());
    assert_eq!(
        outcome.records.get("lithium"),
        Some(&ModRecord {
            version_id: "v1".into(),
            mc_version: "1.21.4".into(),
            loader: "fabric".into(),
            file_path: "mods/lithium-1.21.4.jar".into(),
        })
    );
}

/// The record stores the Homerun loader, not the Modrinth facet a Spigot
/// server resolved against — it describes the server, not the download.
#[test]
fn the_record_names_the_servers_loader_not_the_facet() {
    let mut host = Host::answering(vec![
        (
            "/project/essentials/version",
            json!([version("ess01", "v1", "essentials.jar", "release", &[])]),
        ),
        ("/projects?ids=", json!([project("ess01", "required")])),
    ]);

    let outcome = drive(inputs("spigot", "essentials"), &mut host);
    let record = outcome.records.get("essentials").unwrap();
    assert_eq!(record.loader, "spigot");
    assert_eq!(record.file_path, "plugins/essentials.jar");
    assert!(host.requested.iter().any(|u| u.contains("%22paper%22")));
}

#[test]
fn an_entry_may_pin_an_exact_version_id() {
    let mut host = Host::answering(vec![
        (
            "/version/abc123",
            version("cre01", "abc123", "create-pinned.jar", "release", &[]),
        ),
        ("/projects?ids=", json!([project("cre01", "required")])),
    ]);

    let outcome = drive(inputs("fabric", "create:abc123"), &mut host);

    assert_eq!(outcome.installed, vec!["create"]);
    assert_eq!(host.downloaded, vec!["create-pinned.jar"]);
    assert!(
        host.requested.iter().any(|u| u.contains("/version/abc123")),
        "a pin must fetch the version directly: {:?}",
        host.requested
    );
}

#[test]
fn entries_split_on_commas_and_newlines_and_excluded_slugs_are_skipped() {
    let mut host = Host::answering(vec![
        (
            "/project/lithium/version",
            json!([version("lith01", "v1", "lithium.jar", "release", &[])]),
        ),
        (
            "/project/starlight/version",
            json!([version("star01", "v2", "starlight.jar", "release", &[])]),
        ),
        (
            "/projects?ids=",
            json!([project("lith01", "required"), project("star01", "required")]),
        ),
    ]);

    let mut input = inputs("fabric", "lithium,\n starlight \n, phosphor");
    input.excluded = "PHOSPHOR".into();
    let outcome = drive(input, &mut host);

    assert_eq!(outcome.installed, vec!["lithium", "starlight"]);
    assert!(
        !host.requested.iter().any(|u| u.contains("phosphor")),
        "an excluded slug should never be looked up"
    );
}

// ---------------------------------------------------------------------------
// Version-type fallback
// ---------------------------------------------------------------------------

/// Geyser publishes only betas and C2ME only alphas. Filtering to `release`
/// resolved them to nothing and installed nothing, silently.
#[test]
fn a_beta_is_taken_when_there_is_no_release_and_an_alpha_when_there_is_neither() {
    let mut host = Host::answering(vec![
        (
            "/project/geyser/version",
            json!([
                version("gey01", "beta-new", "geyser-beta.jar", "beta", &[]),
                version("gey01", "beta-old", "geyser-older.jar", "beta", &[]),
            ]),
        ),
        ("/projects?ids=", json!([project("gey01", "required")])),
    ]);
    let outcome = drive(inputs("fabric", "geyser"), &mut host);
    assert_eq!(outcome.records["geyser"].version_id, "beta-new");

    let mut host = Host::answering(vec![
        (
            "/project/c2me/version",
            json!([version("c2m01", "alpha-1", "c2me-alpha.jar", "alpha", &[])]),
        ),
        ("/projects?ids=", json!([project("c2m01", "required")])),
    ]);
    let outcome = drive(inputs("fabric", "c2me"), &mut host);
    assert_eq!(outcome.records["c2me"].version_id, "alpha-1");
}

/// A stable release is still preferred whenever one exists, wherever it sits.
#[test]
fn a_release_wins_over_a_newer_beta() {
    let mut host = Host::answering(vec![
        (
            "/project/mixed/version",
            json!([
                version("mix01", "beta-newest", "mixed-beta.jar", "beta", &[]),
                version(
                    "mix01",
                    "release-older",
                    "mixed-release.jar",
                    "release",
                    &[]
                ),
            ]),
        ),
        ("/projects?ids=", json!([project("mix01", "required")])),
    ]);
    let outcome = drive(inputs("fabric", "mixed"), &mut host);
    assert_eq!(outcome.records["mixed"].version_id, "release-older");
}

// ---------------------------------------------------------------------------
// Client-only mods, and their dependencies
// ---------------------------------------------------------------------------

/// A client-only mod crashes a dedicated server, and the player did nothing
/// wrong by adding one — so it is skipped rather than reported as a failure.
#[test]
fn a_client_only_mod_is_skipped_silently() {
    let mut host = Host::answering(vec![
        (
            "/project/sodium/version",
            json!([version("sod01", "v1", "sodium.jar", "release", &[])]),
        ),
        ("/projects?ids=", json!([project("sod01", "unsupported")])),
    ]);

    let outcome = drive(inputs("fabric", "sodium"), &mut host);

    assert!(outcome.installed.is_empty());
    assert!(
        outcome.failed.is_empty(),
        "not a failure: {:?}",
        outcome.failed
    );
    assert!(host.downloaded.is_empty());
    assert!(outcome.records.is_empty());
}

/// The naive "drop everything client-only" pass breaks servers a different
/// way: a kept mod hard-depends on a client-only library, and removing it
/// fails loader resolution. `chipped` needs `athena`, which is unsupported.
#[test]
fn a_client_only_library_a_kept_mod_requires_is_installed_anyway() {
    let mut host = Host::answering(vec![
        (
            "/project/chipped/version",
            json!([version(
                "chip01",
                "v1",
                "chipped.jar",
                "release",
                &["ath01"]
            )]),
        ),
        (
            "/project/ath01/version",
            json!([version("ath01", "v9", "athena.jar", "release", &[])]),
        ),
        ("/projects?ids=", json!([project("chip01", "required")])),
    ]);

    let outcome = drive(inputs("fabric", "chipped"), &mut host);

    assert_eq!(host.downloaded, vec!["chipped.jar", "athena.jar"]);
    assert_eq!(outcome.records["dep:ath01"].file_path, "mods/athena.jar");
}

#[test]
fn dependencies_are_followed_transitively_and_a_cycle_terminates() {
    let mut host = Host::answering(vec![
        (
            "/project/top/version",
            json!([version("t01", "v1", "top.jar", "release", &["m01"])]),
        ),
        (
            "/project/m01/version",
            json!([version("m01", "v2", "middle.jar", "release", &["b01"])]),
        ),
        (
            "/project/b01/version",
            // Points back at the top, which must not loop.
            json!([version("b01", "v3", "bottom.jar", "release", &["t01"])]),
        ),
        ("/projects?ids=", json!([project("t01", "required")])),
    ]);

    let outcome = drive(inputs("fabric", "top"), &mut host);

    assert_eq!(host.downloaded, vec!["top.jar", "middle.jar", "bottom.jar"]);
    assert!(outcome.records.contains_key("dep:m01"));
    assert!(outcome.records.contains_key("dep:b01"));
}

/// A mod that never arrived must not pull in what it would have needed.
#[test]
fn a_failed_download_does_not_drag_in_its_dependencies() {
    let mut host = Host::answering(vec![
        (
            "/project/broken/version",
            json!([version("brk01", "v1", "broken.jar", "release", &["dep01"])]),
        ),
        (
            "/project/dep01/version",
            json!([version("dep01", "v2", "needed.jar", "release", &[])]),
        ),
        ("/projects?ids=", json!([project("brk01", "required")])),
    ]);
    host.unfetchable.push("broken.jar".into());

    let outcome = drive(inputs("fabric", "broken"), &mut host);

    assert_eq!(host.downloaded, vec!["broken.jar"]);
    assert_eq!(
        outcome.failed,
        vec![Failed {
            slug: "broken".into(),
            reason: "download_failed".into()
        }]
    );
    assert!(!outcome.records.contains_key("dep:dep01"));
}

/// A modpack's own mods are already in `mods/`. Fetching one again as a
/// dependency would leave two copies of it and a duplicate-mod conflict.
#[test]
fn a_project_the_modpack_provides_is_never_pulled_in_as_a_dependency() {
    let mut host = Host::answering(vec![
        (
            "/project/top/version",
            json!([version("t01", "v1", "top.jar", "release", &["packed01"])]),
        ),
        ("/projects?ids=", json!([project("t01", "required")])),
    ]);

    let mut input = inputs("fabric", "top");
    input.modpack_projects = vec!["packed01".into()];
    let outcome = drive(input, &mut host);

    assert_eq!(host.downloaded, vec!["top.jar"]);
    assert!(!outcome.records.keys().any(|k| k.contains("packed01")));
}

// ---------------------------------------------------------------------------
// Failing well
// ---------------------------------------------------------------------------

/// Without the dependency graph, dropping a client-only mod could strip a hard
/// dependency of one we keep. So a failed side lookup installs everything.
#[test]
fn a_failed_server_side_lookup_installs_everything_as_is() {
    let mut host = Host::answering(vec![(
        "/project/sodium/version",
        json!([version("sod01", "v1", "sodium.jar", "release", &[])]),
    )]);
    host.broken.push("/projects?ids=".into());

    let outcome = drive(inputs("fabric", "sodium"), &mut host);

    assert_eq!(outcome.installed, vec!["sodium"]);
    assert_eq!(host.downloaded, vec!["sodium.jar"]);
}

#[test]
fn a_project_with_no_matching_build_is_incompatible_and_one_with_none_at_all_is_not_released() {
    let mut host = Host::answering(vec![
        ("/project/picky/version?", json!([])),
        // The classification request has no query string.
        (
            "/project/picky/version",
            json!([{ "id": "some-old-build" }]),
        ),
    ]);
    let outcome = drive(inputs("fabric", "picky"), &mut host);
    assert_eq!(
        outcome.failed,
        vec![Failed {
            slug: "picky".into(),
            reason: "incompatible".into()
        }]
    );

    let mut host = Host::answering(vec![("/project/ghost/version", json!([]))]);
    let outcome = drive(inputs("fabric", "ghost"), &mut host);
    assert_eq!(
        outcome.failed,
        vec![Failed {
            slug: "ghost".into(),
            reason: "no_release_version".into()
        }]
    );
}

/// A transient Modrinth error must not delete a mod that works.
#[test]
fn a_mod_that_fails_to_resolve_keeps_its_record_and_its_jar() {
    let mut host = Host::default();
    host.broken.push("/project/lithium/version".into());

    let mut input = inputs("fabric", "lithium");
    input.existing = BTreeMap::from([(
        "lithium".to_string(),
        ModRecord {
            version_id: "v-old".into(),
            mc_version: "1.21.4".into(),
            loader: "fabric".into(),
            file_path: "mods/lithium-old.jar".into(),
        },
    )]);
    input.present = vec!["lithium-old.jar".into()];

    let outcome = drive(input, &mut host);

    assert_eq!(outcome.failed[0].reason, "download_failed");
    assert!(
        outcome.records.contains_key("lithium"),
        "the record must survive"
    );
    assert!(
        outcome.remove.is_empty(),
        "a working mod must not be swept: {:?}",
        outcome.remove
    );
}

// ---------------------------------------------------------------------------
// What is already on disk
// ---------------------------------------------------------------------------

/// The version id is what makes this safe. Checking only that a file with the
/// right name exists reuses a jar built for a previous Minecraft version.
#[test]
fn a_mod_already_at_the_resolved_version_is_not_downloaded_again() {
    let mut host = Host::answering(vec![
        (
            "/project/lithium/version",
            json!([version("lith01", "v1", "lithium.jar", "release", &[])]),
        ),
        ("/projects?ids=", json!([project("lith01", "required")])),
    ]);

    let mut input = inputs("fabric", "lithium");
    input.existing = BTreeMap::from([(
        "lithium".to_string(),
        ModRecord {
            version_id: "v1".into(),
            mc_version: "1.21.4".into(),
            loader: "fabric".into(),
            file_path: "mods/lithium.jar".into(),
        },
    )]);
    input.present = vec!["lithium.jar".into()];

    let outcome = drive(input, &mut host);

    assert!(host.downloaded.is_empty(), "already up to date");
    assert!(outcome.remove.is_empty(), "and not stale either");
    assert_eq!(outcome.installed, vec!["lithium"]);
}

#[test]
fn a_recorded_jar_at_the_wrong_version_is_re_downloaded() {
    let mut host = Host::answering(vec![
        (
            "/project/lithium/version",
            json!([version("lith01", "v2", "lithium-new.jar", "release", &[])]),
        ),
        ("/projects?ids=", json!([project("lith01", "required")])),
    ]);

    let mut input = inputs("fabric", "lithium");
    input.existing = BTreeMap::from([(
        "lithium".to_string(),
        ModRecord {
            version_id: "v1".into(),
            mc_version: "1.21.3".into(),
            loader: "fabric".into(),
            file_path: "mods/lithium-old.jar".into(),
        },
    )]);
    input.present = vec!["lithium-old.jar".into()];

    let outcome = drive(input, &mut host);

    assert_eq!(host.downloaded, vec!["lithium-new.jar"]);
    assert_eq!(
        outcome.remove,
        vec!["lithium-old.jar"],
        "the old one is Homerun's to sweep"
    );
}

// ---------------------------------------------------------------------------
// The sweep
// ---------------------------------------------------------------------------

/// The property the sweep exists to protect. A jar the player dropped in by
/// hand has no record naming it, so it is not managed and is never touched.
#[test]
fn the_sweep_only_removes_files_homerun_installed() {
    let present = BTreeSet::from([
        "kept.jar".to_string(),
        "hand-added.jar".to_string(),
        "stale.jar".to_string(),
        "from-a-pack.jar".to_string(),
        "notes.txt".to_string(),
    ]);
    let expected = BTreeSet::from(["kept.jar".to_string()]);
    let preserved = BTreeSet::from(["from-a-pack.jar".to_string()]);
    let managed = BTreeSet::from([
        "kept.jar".to_string(),
        "stale.jar".to_string(),
        "from-a-pack.jar".to_string(),
    ]);

    assert_eq!(
        sweep(&present, &expected, &preserved, &managed),
        vec!["stale.jar".to_string()]
    );
}

#[test]
fn the_sweep_ignores_anything_that_is_not_a_jar() {
    let present = BTreeSet::from(["config.json".to_string(), "old.JAR".to_string()]);
    let managed = BTreeSet::from(["config.json".to_string(), "old.JAR".to_string()]);
    assert_eq!(
        sweep(&present, &BTreeSet::new(), &BTreeSet::new(), &managed),
        vec!["old.JAR".to_string()],
        "case is not what decides it"
    );
}

/// A mod skipped as client-only this run, but installed by a previous one, is
/// left behind unless it is swept — and it is Homerun's to sweep.
#[test]
fn a_mod_that_became_client_only_is_cleaned_up() {
    let mut host = Host::answering(vec![
        (
            "/project/sodium/version",
            json!([version("sod01", "v2", "sodium-new.jar", "release", &[])]),
        ),
        ("/projects?ids=", json!([project("sod01", "unsupported")])),
    ]);

    let mut input = inputs("fabric", "sodium");
    input.existing = BTreeMap::from([(
        "sodium".to_string(),
        ModRecord {
            version_id: "v1".into(),
            mc_version: "1.21.4".into(),
            loader: "fabric".into(),
            file_path: "mods/sodium-old.jar".into(),
        },
    )]);
    input.present = vec!["sodium-old.jar".into()];

    let outcome = drive(input, &mut host);

    assert_eq!(outcome.remove, vec!["sodium-old.jar"]);
    assert!(outcome.records.is_empty());
}

// ---------------------------------------------------------------------------
// Nothing to do
// ---------------------------------------------------------------------------

#[test]
fn no_listed_mods_still_sweeps_what_homerun_left_behind() {
    let mut host = Host::default();
    let mut input = inputs("fabric", "");
    input.existing = BTreeMap::from([(
        "gone".to_string(),
        ModRecord {
            version_id: "v1".into(),
            mc_version: "1.21.4".into(),
            loader: "fabric".into(),
            file_path: "mods/gone.jar".into(),
        },
    )]);
    input.present = vec!["gone.jar".into(), "someone-elses.jar".into()];

    let outcome = drive(input, &mut host);

    assert_eq!(outcome.remove, vec!["gone.jar"]);
    assert!(host.requested.is_empty(), "and asks Modrinth nothing");
}

// ---------------------------------------------------------------------------
// URL shapes
// ---------------------------------------------------------------------------

/// Byte-identical to what the desktop builds, because the same endpoint has to
/// answer the same question. `encodeURIComponent` leaves more alone than a
/// default percent-encoder does.
#[test]
fn a_version_query_is_encoded_the_way_the_desktop_encodes_it() {
    let url = version_list_url("fabric-api", "1.21.4", "fabric");
    assert_eq!(
        url,
        "https://api.modrinth.com/v2/project/fabric-api/version\
         ?game_versions=%5B%221.21.4%22%5D&loaders=%5B%22fabric%22%5D"
    );
}

#[test]
fn encode_leaves_the_unreserved_set_alone() {
    assert_eq!(encode("a-b_c.d!e~f*g'h(i)"), "a-b_c.d!e~f*g'h(i)");
    assert_eq!(encode("[\"x\"]"), "%5B%22x%22%5D");
    assert_eq!(encode("a b/c"), "a%20b%2Fc");
}

// ---------------------------------------------------------------------------
// The shared cases
// ---------------------------------------------------------------------------

/// Run every case in `shared/fixtures/mods/`.
///
/// Those files are the anti-drift mechanism, not a second copy of the tests
/// above: they are meant to be run against the desktop's implementation too,
/// so that a behaviour learned in one is enforced in both. See the README
/// beside them.
///
/// A missing directory fails rather than skipping. A shared contract that
/// quietly stops being checked is worse than not having one.
#[test]
fn every_shared_case_produces_the_answer_it_records() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../shared/fixtures/mods");
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("shared/fixtures/mods is unreadable ({e}); it is the contract"));

    let mut ran = 0;
    for entry in entries {
        let path = entry.expect("readable entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        run_case(&path);
        ran += 1;
    }
    assert!(ran > 0, "no cases found in {dir}");
}

fn run_case(path: &std::path::Path) {
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{name}: {e}"));
    let case: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"));

    let why = case["why"].as_str().unwrap_or("(no reason recorded)");
    let inputs: Inputs = serde_json::from_value(case["inputs"].clone())
        .unwrap_or_else(|e| panic!("{name}: bad inputs: {e}"));

    let mut host = Host {
        json: case["modrinth"]
            .as_object()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default(),
        unfetchable: case["unfetchable"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        ..Default::default()
    };

    let outcome = drive(inputs, &mut host);
    let expect = &case["expect"];

    let strings = |v: &Value| -> Vec<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };

    assert_eq!(
        outcome.installed,
        strings(&expect["installed"]),
        "{name}: {why}"
    );
    assert_eq!(
        host.downloaded,
        strings(&expect["downloaded"]),
        "{name}: {why}"
    );
    assert_eq!(outcome.remove, strings(&expect["remove"]), "{name}: {why}");
    assert_eq!(
        serde_json::to_value(&outcome.failed).unwrap(),
        if expect["failed"].is_null() {
            json!([])
        } else {
            expect["failed"].clone()
        },
        "{name}: {why}"
    );
    assert_eq!(
        serde_json::to_value(&outcome.records).unwrap(),
        if expect["records"].is_null() {
            json!({})
        } else {
            expect["records"].clone()
        },
        "{name}: {why}"
    );
}

#[test]
fn ids_are_batched_a_hundred_at_a_time() {
    let many: Vec<String> = (0..250).map(|i| format!("p{i:03}")).collect();
    let mut host = Host::answering(vec![("/projects?ids=", json!([]))]);
    for id in &many {
        host.json.push((
            format!("/project/{id}/version"),
            json!([version(id, "v1", &format!("{id}.jar"), "release", &[])]),
        ));
    }

    drive(inputs("fabric", &many.join(",")), &mut host);

    let batches = host
        .requested
        .iter()
        .filter(|u| u.contains("/projects?ids="))
        .count();
    assert_eq!(batches, 3, "250 ids is three calls, not 250");
}
