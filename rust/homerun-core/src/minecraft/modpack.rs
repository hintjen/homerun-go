//! Installing a Modrinth modpack.
//!
//! # What a `.mrpack` is
//!
//! A zip with `modrinth.index.json` — the loader, the Minecraft version, and a
//! list of mods to fetch by URL — plus an `overrides/` tree copied verbatim
//! into the server directory. Some of a pack's mods arrive as URLs in the
//! manifest and some as jars inside `overrides/mods/`, and the two need
//! different handling for the same question.
//!
//! # The question
//!
//! **Which of these mods must not be installed on a dedicated server?**
//!
//! A client-only mod crashes one, usually with a mixin that targets a
//! client-only class. The manifest's own per-file `env.server` is *not* a
//! reliable answer: it is author-supplied, and kitchen-sink packs routinely
//! export every mod as `env.server: "required"` even when many are client-only.
//! So Modrinth's project-level `server_side` is consulted instead.
//!
//! And then the naive answer — drop everything client-only — breaks servers a
//! different way, because a kept server mod often **hard-depends** on a
//! client-only library. So a dependency closure keeps those. See
//! `native-mod-support.md`, which documents all of this and two failed
//! attempts at doing better.
//!
//! # Provenance
//!
//! `setupModrinthModpack` (`mod-installer.ts:1272`). This is the decisions;
//! the host downloads the pack, reads the zip and writes the files.

use super::modjar::{Facts, Side};
use super::mods::{Reply, Step};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const API: &str = "https://api.modrinth.com/v2";

/// Ids per `/projects?ids=[…]` call, as Modrinth documents.
const IDS_PER_CALL: usize = 100;

// ---------------------------------------------------------------------------
// Finding the pack
// ---------------------------------------------------------------------------

/// Where a `.mrpack` lives, and what to call it in the cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Source {
    pub url: String,
    /// A stable filename for the cached download.
    #[serde(rename = "cacheKey")]
    pub cache_key: String,
}

/// What to do about a `MODRINTH_MODPACK` value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Plan {
    /// Already a URL to the archive; fetch it.
    Ready { source: Source },
    /// Ask Modrinth, then call [`source_from`] with the answer.
    Ask { url: String, of: Lookup },
}

/// Which shape of Modrinth answer [`Plan::Ask`] will produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Lookup {
    /// One version, named in the URL.
    Version,
    /// The project's versions, newest first.
    Latest,
}

/// Turn a `MODRINTH_MODPACK` value into the next step.
///
/// Three forms, all of which appear on real servers:
///
/// | Value | Meaning |
/// |---|---|
/// | `modrinth.com/modpack/<slug>/version/<id>` | that exact version |
/// | `modrinth.com/modpack/<slug>` | its latest release |
/// | anything else | a direct URL to the archive |
pub fn plan(modpack: &str) -> Plan {
    let trimmed = modpack.trim();

    if let Some(rest) = after(trimmed, "modrinth.com/modpack/") {
        let slug = until(rest, &['/', '?', '#']);
        if let Some(version) = after(rest, "/version/") {
            let id = until(version, &['/', '?', '#']);
            return Plan::Ask {
                url: format!("{API}/version/{}", encode(id)),
                of: Lookup::Version,
            };
        }
        // `featured=true` first, exactly as the desktop asks — a pack's
        // featured release is what its page shows, and falling back to the
        // full list is the second request rather than the first.
        return Plan::Ask {
            url: format!(
                "{API}/project/{}/version?version_type=release&featured=true",
                encode(slug)
            ),
            of: Lookup::Latest,
        };
    }

    Plan::Ready {
        source: Source {
            url: trimmed.to_string(),
            cache_key: cache_key_for(trimmed),
        },
    }
}

/// Read the archive URL out of what [`Plan::Ask`] fetched.
///
/// For [`Lookup::Latest`], an empty answer is not a failure — it means the
/// pack has no *featured release*, and the caller retries with
/// [`fallback_versions_url`] before giving up. Some packs only ever publish
/// betas.
pub fn source_from(of: Lookup, json: &Value) -> Result<Option<Source>> {
    let version = match of {
        Lookup::Version => json,
        Lookup::Latest => {
            let versions = json
                .as_array()
                .ok_or_else(|| Error::Malformed("the modpack version list is not a list".into()))?;
            match pick_version(versions) {
                Some(v) => v,
                None => return Ok(None),
            }
        }
    };

    let id = version
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Malformed("the modpack version has no id".into()))?;
    let files = version
        .get("files")
        .and_then(|v| v.as_array())
        .ok_or_else(|| Error::Malformed("the modpack version has no files".into()))?;
    let primary = files
        .iter()
        .find(|f| f.get("primary").and_then(Value::as_bool) == Some(true))
        .or_else(|| files.first())
        .and_then(|f| f.get("url"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Malformed("the modpack version publishes no archive".into()))?;

    Ok(Some(Source {
        url: primary.to_string(),
        // The version id, so a pack pinned to one version is cached once and a
        // changed pin is a different file rather than a stale one.
        cache_key: id.to_string(),
    }))
}

/// Prefer a release, then the newest beta, then whatever is newest.
fn pick_version(versions: &[Value]) -> Option<&Value> {
    let of_type = |kind: &str| {
        versions
            .iter()
            .find(|v| v.get("version_type").and_then(Value::as_str) == Some(kind))
    };
    of_type("release")
        .or_else(|| of_type("beta"))
        .or_else(|| versions.first())
}

/// The unfiltered version list, for a pack with no featured release.
pub fn fallback_versions_url(modpack: &str) -> Option<String> {
    let rest = after(modpack.trim(), "modrinth.com/modpack/")?;
    let slug = until(rest, &['/', '?', '#']);
    Some(format!("{API}/project/{}/version", encode(slug)))
}

/// A stable, filesystem-safe name for a directly-linked archive.
///
/// FNV-1a, and **not** [`crate::md5`] — that module says in as many words not
/// to reach for it outside the offline-UUID derivation it exists for. Nothing
/// here needs a cryptographic hash: this is a local cache filename, it never
/// leaves the device, and it does not have to match what the desktop chose for
/// the same URL.
fn cache_key_for(url: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in url.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{hash:016x}")
}

// ---------------------------------------------------------------------------
// Reading the manifest
// ---------------------------------------------------------------------------

/// What a pack says it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Requires {
    pub loader: String,
    #[serde(rename = "mcVersion")]
    pub mc_version: String,
    /// The exact loader build the pack was tested against, if it pinned one.
    #[serde(rename = "loaderVersion", skip_serializing_if = "Option::is_none")]
    pub loader_version: Option<String>,
}

/// Which loader a pack needs, and which build of it.
///
/// The priority is the desktop's — `neoforge`, then `forge`, then
/// `quilt-loader`, then `fabric-loader`, defaulting to Fabric — and it matters
/// because a pack can name more than one.
///
/// **The pinned build is not a detail.** A pack is built and tested against a
/// specific loader revision, and version-sensitive mixins target the exact
/// patched classes of that revision. Installing a different one shifts those
/// classes and breaks injection at boot: a Forge-1.20.1 pack pinned to
/// `47.2.17` and run on `47.4.20` dies with `InjectionError: … (0/1) succeeded`.
pub fn requires(manifest: &Value) -> Result<Requires> {
    let deps = manifest
        .get("dependencies")
        .and_then(|v| v.as_object())
        .ok_or_else(|| Error::Malformed("the modpack manifest names no dependencies".into()))?;

    let text = |key: &str| deps.get(key).and_then(|v| v.as_str()).map(str::to_string);

    let mc_version = text("minecraft").ok_or_else(|| {
        Error::Malformed("the modpack manifest names no Minecraft version".into())
    })?;

    let (loader, loader_version) = if let Some(v) = text("neoforge") {
        ("neoforge", Some(v))
    } else if let Some(v) = text("forge") {
        ("forge", Some(v))
    } else if let Some(v) = text("quilt-loader") {
        ("quilt", Some(v))
    } else {
        ("fabric", text("fabric-loader"))
    };

    Ok(Requires {
        loader: loader.to_string(),
        mc_version,
        loader_version,
    })
}

// ---------------------------------------------------------------------------
// Deciding what to install
// ---------------------------------------------------------------------------

/// One mod the pack ships, from either source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackFile {
    /// The basename, which is what everything downstream keys on.
    pub filename: String,
    /// `sha512` from the manifest, or the host's hash of an override jar.
    #[serde(default)]
    pub sha512: Option<String>,
    /// The first entry of `downloads`; absent for an override.
    #[serde(default)]
    pub url: Option<String>,
    /// A project id parsed out of a `cdn.modrinth.com` URL, as a fallback.
    #[serde(default, rename = "urlProjectId")]
    pub url_project_id: Option<String>,
    /// What the jar says about itself. Only read for overrides.
    #[serde(default)]
    pub facts: Option<Facts>,
}

/// Everything the host knows before any request.
#[derive(Debug, Clone, Deserialize)]
pub struct Inputs {
    /// Mods listed in `modrinth.index.json` under `mods/`.
    pub manifest: Vec<PackFile>,
    /// Jars found in `overrides/mods/`.
    #[serde(default)]
    pub overrides: Vec<PackFile>,
    /// `MODRINTH_EXCLUDE_FILES`, verbatim.
    #[serde(default, rename = "excludeFiles")]
    pub exclude_files: String,
    /// `MODRINTH_OVERRIDES_EXCLUSIONS`, verbatim.
    #[serde(default, rename = "overridesExclusions")]
    pub overrides_exclusions: String,
}

/// What the host must do to the server directory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Outcome {
    /// Manifest mods to fetch, in order.
    pub download: Vec<Fetch>,
    /// Override jars to skip when extracting, by filename.
    #[serde(rename = "skipOverrides")]
    pub skip_overrides: Vec<String>,
    /// Mod filenames to delete, having been excluded this run.
    pub remove: Vec<String>,
    /// Every mod filename the pack placed — `modpackFiles` in the marker.
    pub files: Vec<String>,
    /// Project ids the pack provides, so `mods` never re-installs one.
    pub projects: Vec<String>,
    /// One line per exclusion, for the console.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Fetch {
    pub url: String,
    pub filename: String,
}

/// Where the driver is.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Progress {
    Steps { steps: Vec<Step>, state: Session },
    Done { outcome: Outcome },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Phase {
    Versions,
    Sides,
    Naming,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum Waiting {
    Versions,
    Sides,
    Naming { filename: String },
}

/// The driver's state, carried across calls as opaque JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    manifest: Vec<PackFile>,
    overrides: Vec<PackFile>,
    exclude_files: Vec<String>,
    overrides_exclusions: Vec<String>,

    phase: Phase,
    waiting: std::collections::BTreeMap<String, Waiting>,
    next_id: u32,

    /// sha512 -> project id, from `/version_files`.
    project_by_hash: std::collections::BTreeMap<String, String>,
    /// project id -> its required dependencies.
    deps: std::collections::BTreeMap<String, Vec<String>>,
    /// Whether the version lookup succeeded at all. See [`Session::finish`].
    resolved: bool,
    /// project ids Modrinth marks `server_side: unsupported`.
    unsupported: Vec<String>,
    /// Override filenames excluded by their own metadata or by name search.
    excluded_overrides: Vec<String>,
}

/// Start deciding what a pack installs.
pub fn begin(inputs: Inputs) -> Progress {
    let mut session = Session {
        manifest: inputs.manifest,
        overrides: inputs.overrides,
        exclude_files: split_list(&inputs.exclude_files),
        overrides_exclusions: split_list(&inputs.overrides_exclusions),
        phase: Phase::Versions,
        waiting: Default::default(),
        next_id: 0,
        project_by_hash: Default::default(),
        deps: Default::default(),
        resolved: false,
        unsupported: Vec::new(),
        excluded_overrides: Vec::new(),
    };

    let hashes: Vec<String> = session
        .manifest
        .iter()
        .chain(session.overrides.iter())
        .filter_map(|f| f.sha512.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();

    if hashes.is_empty() {
        // Nothing to look up means nothing can be judged, which is the
        // fail-open case rather than an empty pack.
        return session.finish();
    }

    // One call for every mod in the pack, by hash. A 192-mod pack is one
    // request here and two more below, not 192.
    let id = session.claim_id();
    session.waiting.insert(id.clone(), Waiting::Versions);
    let step = Step::Json {
        id,
        url: format!(
            "{API}/version_files?algorithm=sha512&hashes={}",
            encode(&json_array(&hashes))
        ),
    };
    Progress::Steps {
        steps: vec![step],
        state: session,
    }
}

/// Feed back what the host fetched.
pub fn advance(session: Session, replies: Vec<Reply>) -> Progress {
    match session.phase {
        Phase::Versions => session.after_versions(replies),
        Phase::Sides => session.after_sides(replies),
        Phase::Naming => session.after_naming(replies),
    }
}

impl Session {
    fn claim_id(&mut self) -> String {
        self.next_id += 1;
        format!("p{}", self.next_id)
    }

    fn take(&mut self, replies: &[Reply]) -> Vec<(Waiting, Reply)> {
        let paired = replies
            .iter()
            .filter_map(|r| self.waiting.get(&r.id).cloned().map(|w| (w, r.clone())))
            .collect();
        self.waiting.clear();
        paired
    }

    fn after_versions(mut self, replies: Vec<Reply>) -> Progress {
        for (_, reply) in self.take(&replies) {
            let Some(map) = reply.json.as_ref().and_then(|j| j.as_object()) else {
                continue;
            };
            self.resolved = true;
            for (hash, version) in map {
                let Some(pid) = version.get("project_id").and_then(|v| v.as_str()) else {
                    continue;
                };
                self.project_by_hash.insert(hash.clone(), pid.to_string());
                let required: Vec<String> = version
                    .get("dependencies")
                    .and_then(|v| v.as_array())
                    .map(|deps| {
                        deps.iter()
                            .filter(|d| {
                                d.get("dependency_type").and_then(Value::as_str) == Some("required")
                            })
                            .filter_map(|d| d.get("project_id").and_then(Value::as_str))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                self.deps.insert(pid.to_string(), required);
            }
        }

        // **Fail-safe, not fail-open, and this direction is deliberate.**
        // Without the dependency graph, excluding a client-only mod could
        // strip a hard dependency of one being kept — so a failed lookup
        // installs the pack exactly as its author shipped it.
        if !self.resolved {
            return self.finish();
        }

        let ids: Vec<String> = self
            .project_by_hash
            .values()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        // No ids is not "nothing to do": a pack whose mods are *all*
        // off-Modrinth CurseForge builds resolves no hashes at all, and those
        // are exactly the jars the name search exists for. Skipping to the end
        // here is how a client-only override gets installed.
        if ids.is_empty() {
            return self.name_search();
        }

        self.phase = Phase::Sides;
        let steps: Vec<Step> = ids
            .chunks(IDS_PER_CALL)
            .map(|chunk| {
                let id = self.claim_id();
                self.waiting.insert(id.clone(), Waiting::Sides);
                Step::Json {
                    id,
                    url: format!("{API}/projects?ids={}", encode(&json_array(chunk))),
                }
            })
            .collect();
        Progress::Steps { steps, state: self }
    }

    fn after_sides(mut self, replies: Vec<Reply>) -> Progress {
        for (_, reply) in self.take(&replies) {
            let Some(projects) = reply.json.as_ref().and_then(|j| j.as_array()) else {
                continue;
            };
            for project in projects {
                let id = project.get("id").and_then(|v| v.as_str());
                let side = project.get("server_side").and_then(|v| v.as_str());
                if let (Some(id), Some("unsupported")) = (id, side) {
                    if !self.unsupported.iter().any(|u| u == id) {
                        self.unsupported.push(id.to_string());
                    }
                }
            }
        }
        self.name_search()
    }

    /// Ask about override jars Modrinth's hashes could not identify.
    ///
    /// These are the CurseForge builds a pack ships directly. A jar that
    /// **declares itself client-only** is excluded without asking anyone; for
    /// every other one — including those declaring `side = "BOTH"`, which
    /// authors leave on genuinely client-only mods — the mod id goes to
    /// Modrinth's search, and an exact-slug match to a client-only *mod* is
    /// authoritative.
    fn name_search(mut self) -> Progress {
        let mut steps = Vec::new();
        let mut declared_client = Vec::new();

        for file in self.overrides.clone() {
            let known = file
                .sha512
                .as_ref()
                .is_some_and(|h| self.project_by_hash.contains_key(h));
            if known {
                continue;
            }
            let facts = file.facts.clone().unwrap_or_default();
            if facts.side == Side::Client {
                declared_client.push(file.filename.clone());
                continue;
            }
            let Some(mod_id) = facts.mod_id else { continue };
            let id = self.claim_id();
            self.waiting.insert(
                id.clone(),
                Waiting::Naming {
                    filename: file.filename.clone(),
                },
            );
            steps.push(Step::Json {
                id,
                url: format!("{API}/search?query={}&limit=1", encode(&mod_id)),
            });
        }

        self.excluded_overrides.extend(declared_client);

        if steps.is_empty() {
            return self.finish();
        }
        self.phase = Phase::Naming;
        Progress::Steps { steps, state: self }
    }

    fn after_naming(mut self, replies: Vec<Reply>) -> Progress {
        for (waiting, reply) in self.take(&replies) {
            let Waiting::Naming { filename } = waiting else {
                continue;
            };
            let Some(hit) = reply
                .json
                .as_ref()
                .and_then(|j| j.get("hits"))
                .and_then(|h| h.as_array())
                .and_then(|h| h.first())
            else {
                continue;
            };

            // Three guards, and each has caught a false positive. The hit must
            // be a *mod* — the `taa` shaderpack must not strip a same-named
            // Forge mod — its slug must normalise-equal the mod id, so a
            // non-matching top hit (`framework` -> `modern-ui`) is ignored,
            // and only then does its `server_side` count.
            if hit.get("project_type").and_then(Value::as_str) != Some("mod") {
                continue;
            }
            let slug = hit.get("slug").and_then(Value::as_str).unwrap_or_default();
            let wanted = self
                .overrides
                .iter()
                .find(|f| f.filename == filename)
                .and_then(|f| f.facts.as_ref())
                .and_then(|f| f.mod_id.as_deref())
                .unwrap_or_default();
            if normalise(slug) != normalise(wanted) {
                continue;
            }
            if hit.get("server_side").and_then(Value::as_str) == Some("unsupported")
                && !self.excluded_overrides.contains(&filename)
            {
                self.excluded_overrides.push(filename);
            }
        }
        self.finish()
    }

    fn finish(self) -> Progress {
        Progress::Done {
            outcome: self.decide(),
        }
    }

    fn decide(&self) -> Outcome {
        let mut out = Outcome::default();

        // Every project the pack ships, and the ones no kept mod needs.
        let all: Vec<String> = self
            .project_by_hash
            .values()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();

        // The closure. Start from everything not marked unsupported and pull
        // in whatever those require, keeping client-only libraries a kept mod
        // hard-depends on — `chipped` needs `athena`, which is unsupported.
        let mut kept: Vec<String> = all
            .iter()
            .filter(|p| !self.unsupported.contains(p))
            .cloned()
            .collect();
        let mut stack = kept.clone();
        while let Some(project) = stack.pop() {
            for dep in self.deps.get(&project).cloned().unwrap_or_default() {
                if all.contains(&dep) && !kept.contains(&dep) {
                    kept.push(dep.clone());
                    stack.push(dep);
                }
            }
        }

        let excluded: Vec<String> = if self.resolved {
            self.unsupported
                .iter()
                .filter(|p| !kept.contains(p))
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        let rescued = self.unsupported.len() - excluded.len();
        if !excluded.is_empty() {
            out.notes.push(format!(
                "Excluding {} client-only mod{}{}",
                excluded.len(),
                if excluded.len() == 1 { "" } else { "s" },
                if rescued > 0 {
                    format!("; keeping {rescued} that server mods require")
                } else {
                    String::new()
                },
            ));
        }

        let project_of = |file: &PackFile| -> Option<String> {
            file.sha512
                .as_ref()
                .and_then(|h| self.project_by_hash.get(h))
                .cloned()
                .or_else(|| file.url_project_id.clone())
        };

        // --- manifest mods --------------------------------------------
        for file in &self.manifest {
            let client_only = project_of(file)
                .map(|p| excluded.contains(&p))
                .unwrap_or(false);
            if client_only {
                out.notes
                    .push(format!("Excluding client-only mod: {}", file.filename));
                out.remove.push(file.filename.clone());
                continue;
            }
            if matches_exclude(&file.filename, &self.exclude_files) {
                out.notes.push(format!(
                    "Excluding {} (MODRINTH_EXCLUDE_FILES)",
                    file.filename
                ));
                out.remove.push(file.filename.clone());
                continue;
            }
            let Some(url) = file.url.clone() else {
                continue;
            };
            out.files.push(file.filename.clone());
            out.download.push(Fetch {
                url,
                filename: file.filename.clone(),
            });
        }

        // --- overrides -------------------------------------------------
        for file in &self.overrides {
            let by_project = project_of(file)
                .map(|p| excluded.contains(&p))
                .unwrap_or(false);
            let by_jar = self.excluded_overrides.contains(&file.filename);
            let by_glob = self
                .overrides_exclusions
                .iter()
                .any(|p| ant_matches(p, &format!("mods/{}", file.filename)));

            if by_project || by_jar || by_glob {
                let why = if by_glob {
                    "MODRINTH_OVERRIDES_EXCLUSIONS"
                } else {
                    "client-only"
                };
                out.notes
                    .push(format!("Excluding override {} ({why})", file.filename));
                out.skip_overrides.push(file.filename.clone());
                out.remove.push(file.filename.clone());
                continue;
            }
            out.files.push(file.filename.clone());
        }

        out.projects = all.into_iter().filter(|p| !excluded.contains(p)).collect();
        out
    }
}

// ---------------------------------------------------------------------------
// After assembly
// ---------------------------------------------------------------------------

/// One jar as it now sits in `mods/`.
#[derive(Debug, Clone, Deserialize)]
pub struct Assembled {
    pub filename: String,
    pub facts: Facts,
    /// Whether Modrinth marked this jar's project client-only.
    #[serde(default, rename = "clientOnly")]
    pub client_only: bool,
}

/// Prune rescued client-only mods whose hard dependencies are not installed.
///
/// The closure above can rescue a `server_side: unsupported` mod because a
/// kept mod requires it. But **Modrinth's per-version dependency array drifts
/// from the jar's own metadata**, and the loader enforces the jar: Modrinth
/// lists sodiumoptionsapi as needing only Sodium, while its
/// `neoforge.mods.toml` still hard-requires reeses_sodium_options, which was
/// excluded as client-only. NeoForge dies with "requires
/// reeses_sodium_options … not installed".
///
/// So the assembled directory is validated against what the loader actually
/// reads. A **client-only** mod missing a hard dependency cannot run
/// server-side either, so it is dropped and the drop cascades. A
/// server-installable mod missing one is left alone: that is a genuine,
/// reportable modpack error and not ours to paper over.
pub fn reconcile(jars: &[Assembled]) -> Vec<String> {
    let client_only = |j: &Assembled| j.client_only || j.facts.side == Side::Client;

    let mut present: Vec<String> = jars
        .iter()
        .filter_map(|j| j.facts.mod_id.clone())
        .map(|id| id.to_ascii_lowercase())
        .collect();
    let mut pruned: Vec<String> = Vec::new();

    let mut changed = true;
    while changed {
        changed = false;
        for jar in jars {
            if pruned.contains(&jar.filename) || !client_only(jar) {
                continue;
            }
            let missing = jar
                .facts
                .deps
                .iter()
                .any(|d| !present.contains(&d.to_ascii_lowercase()));
            if !missing {
                continue;
            }
            pruned.push(jar.filename.clone());
            if let Some(id) = &jar.facts.mod_id {
                present.retain(|p| p != &id.to_ascii_lowercase());
            }
            changed = true;
        }
    }
    pruned
}

// ---------------------------------------------------------------------------
// Small shared shapes
// ---------------------------------------------------------------------------

/// `MODRINTH_EXCLUDE_FILES`: each pattern is a **partial** filename, so
/// `rubidium-extra` drops `rubidium-extra-0.4.18.jar`. Mirrors itzg, which is
/// where the variable comes from.
pub fn matches_exclude(filename: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| filename.contains(p.as_str()))
}

/// An ant-style glob, as `MODRINTH_OVERRIDES_EXCLUSIONS` takes them.
///
/// `?` is one non-separator character, `*` stays within a path segment, and
/// `**` crosses them. Matched whole rather than as a search.
pub fn ant_matches(pattern: &str, path: &str) -> bool {
    ant_match_at(pattern.as_bytes(), path.as_bytes())
}

fn ant_match_at(pattern: &[u8], path: &[u8]) -> bool {
    // Index-walking rather than a compiled regex: this crate has no regex, and
    // the grammar is three cases.
    if pattern.is_empty() {
        return path.is_empty();
    }
    match pattern[0] {
        b'*' if pattern.len() > 1 && pattern[1] == b'*' => {
            // `**` matches anything, including separators.
            (0..=path.len()).any(|i| ant_match_at(&pattern[2..], &path[i..]))
        }
        b'*' => (0..=path.len())
            .take_while(|i| path[..*i].iter().all(|c| *c != b'/'))
            .any(|i| ant_match_at(&pattern[1..], &path[i..])),
        b'?' => !path.is_empty() && path[0] != b'/' && ant_match_at(&pattern[1..], &path[1..]),
        c => !path.is_empty() && path[0] == c && ant_match_at(&pattern[1..], &path[1..]),
    }
}

fn normalise(text: &str) -> String {
    text.to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

fn split_list(raw: &str) -> Vec<String> {
    raw.split(['\n', ','])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn after<'a>(text: &'a str, marker: &str) -> Option<&'a str> {
    text.find(marker).map(|i| &text[i + marker.len()..])
}

fn until<'a>(text: &'a str, stops: &[char]) -> &'a str {
    match text.find(stops) {
        Some(i) => &text[..i],
        None => text,
    }
}

fn json_array(items: &[String]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|s| serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\"")))
        .collect();
    format!("[{}]", quoted.join(","))
}

fn encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => out.push(byte as char),
            b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => out.push(byte as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests;
