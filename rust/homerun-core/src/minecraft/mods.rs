//! Which mods a server gets, and where they come from.
//!
//! # Why this is here and not in the host
//!
//! `native-mod-support.md` in the `homerun` repo says it plainly: the desktop
//! carries **two** hand-maintained copies of this pipeline
//! (`nativeServerManager.ts` and `mod-installer.ts`), they are kept in parity
//! by hand, and "a logic fix must be applied to both". A Kotlin port would
//! have made three. This is meant to end up being one.
//!
//! `downloadMods` in `mod-installer.ts` is the spec. Where this deliberately
//! differs it says so; everywhere else, a difference is a bug.
//!
//! # Why it is a step machine
//!
//! This crate is pure — no I/O, no async, no runtime — and installing mods is
//! I/O all the way down: three phases of interleaved HTTP with a graph search
//! in the middle. It cannot be one pure function, so it is a driver.
//!
//! ```text
//! begin(...)              -> Progress::Steps  { steps, state }
//! advance(state, replies) -> Progress::Steps  { steps, state }
//!                          | Progress::Done   { outcome }
//! ```
//!
//! The host performs the steps — Modrinth requests and file downloads — and
//! hands back what happened. Every *decision* stays here: which version wins,
//! what is skipped, which dependency is pulled in, which jar is swept. None of
//! it needs a network to test.
//!
//! Downloads are steps rather than something the host does at the end, and
//! that is not incidental. A mod whose download fails must not pull in its
//! dependencies — the desktop gets that by downloading inside the loop, and
//! resolving everything up front would quietly install dependencies for mods
//! that never arrived.
//!
//! # What is not here
//!
//! **Modpacks** (`setupModrinthModpack`) — M5. **Naive mode**
//! (`disableAutoFix`) — that exists for the desktop's Dockerised KnownError
//! reproducer, to make a crash surface rather than be fixed, and an app has no
//! use for it.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Modrinth's API root. Interpolated into every step's URL.
const API: &str = "https://api.modrinth.com/v2";

/// Ids per `/projects?ids=[…]` call. Modrinth's documented ceiling, and what
/// turns a 192-mod pack into three requests rather than one-per-mod.
const IDS_PER_CALL: usize = 100;

// ---------------------------------------------------------------------------
// What a server directory remembers
// ---------------------------------------------------------------------------

/// One installed mod, as `.homerun-loader.json` records it.
///
/// Its reason for existing is [`ModRecord::version_id`]. Without it the only
/// skip check is "a file with that name is already there", so a jar built for
/// a previous Minecraft version is silently reused — it has the right name and
/// the wrong contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModRecord {
    #[serde(rename = "versionId")]
    pub version_id: String,
    #[serde(rename = "mcVersion")]
    pub mc_version: String,
    pub loader: String,
    #[serde(rename = "filePath")]
    pub file_path: String,
}

/// Why a mod the user asked for is not installed.
///
/// Distinguished because they mean different things to a player: a mod with no
/// build for this Minecraft version is a choice they can change, and a failed
/// download is worth retrying.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failed {
    pub slug: String,
    pub reason: String,
}

impl Failed {
    /// Modrinth has the project, but nothing that fits this loader/version.
    const INCOMPATIBLE: &'static str = "incompatible";
    /// Modrinth has no published version of it at all.
    const NO_RELEASE: &'static str = "no_release_version";
    /// It resolved, and getting it failed.
    const DOWNLOAD_FAILED: &'static str = "download_failed";
}

// ---------------------------------------------------------------------------
// The conversation with the host
// ---------------------------------------------------------------------------

/// One thing the host must do before the core can continue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Step {
    /// `GET [url]`, parse as JSON, hand it back.
    Json { id: String, url: String },
    /// Fetch [url] into the mod directory as [filename].
    Download {
        id: String,
        url: String,
        filename: String,
    },
}

impl Step {
    pub fn id(&self) -> &str {
        match self {
            Step::Json { id, .. } | Step::Download { id, .. } => id,
        }
    }
}

/// What happened when the host performed a [`Step`].
///
/// A reply with neither `json` nor `error` is treated as a failure, which is
/// the safe reading: a step that reported nothing did not demonstrably work.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Reply {
    pub id: String,
    #[serde(default)]
    pub json: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
}

impl Reply {
    fn failed(&self) -> bool {
        self.error.is_some()
    }
}

/// Everything the host must do to the mod directory, once resolution is over.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Outcome {
    /// Slugs that were installed, for the log.
    pub installed: Vec<String>,
    /// What the user asked for and did not get, and why.
    pub failed: Vec<Failed>,
    /// Filenames to delete from the mod directory. See [`sweep`].
    pub remove: Vec<String>,
    /// The `mods` map to write into `.homerun-loader.json`.
    pub records: BTreeMap<String, ModRecord>,
    /// `mods` or `plugins` — which directory all of the above refers to.
    #[serde(rename = "subDir")]
    pub sub_dir: String,
}

/// Where the driver is: more to do, or finished.
///
/// Internally tagged so the host reads one `kind` field rather than unwrapping
/// a single-key object — the same shape [`Step`] uses.
///
/// The variants are deliberately lopsided: `Steps` carries the whole session
/// and `Done` carries only the outcome. Boxing to even them out would buy
/// nothing — exactly one of these exists per round of the driver, and it is
/// serialised to JSON immediately and dropped.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Progress {
    Steps { steps: Vec<Step>, state: Session },
    Done { outcome: Outcome },
}

// ---------------------------------------------------------------------------
// The session
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Phase {
    ResolvingTops,
    ClassifyingFailures,
    FetchingSides,
    InstallingTops,
    ResolvingDeps,
    DownloadingDeps,
}

/// A resolved Modrinth version: the one file to fetch, and what it needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Picked {
    project_id: String,
    version_id: String,
    file_url: String,
    filename: String,
    deps: Vec<String>,
}

/// What an outstanding step was for, so a reply can be interpreted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Waiting {
    TopVersion { slug: String },
    TopClassify { slug: String },
    Sides,
    TopDownload { slug: String, picked: Picked },
    DepVersion { project_id: String },
    DepDownload { project_id: String, picked: Picked },
}

/// The driver's whole state, carried across calls as JSON.
///
/// Opaque to the host: it holds it and hands it back. Serialised rather than
/// kept in a registry so the core stays a pure function of its arguments —
/// which is what lets every test below run the whole pipeline without a
/// network or a lifetime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    loader: String,
    facet: String,
    sub_dir: String,
    game_version: String,
    entries: Vec<(String, String)>,
    existing: BTreeMap<String, ModRecord>,
    managed_before: BTreeSet<String>,
    present: BTreeSet<String>,
    preserved: BTreeSet<String>,

    phase: Phase,
    waiting: BTreeMap<String, Waiting>,
    next_id: u32,

    tops: Vec<(String, Picked)>,
    expected: BTreeSet<String>,
    installed: Vec<String>,
    failed: Vec<Failed>,
    records: BTreeMap<String, ModRecord>,
    unsupported: BTreeSet<String>,
    installed_projects: BTreeSet<String>,
    seen: BTreeSet<String>,
    dep_queue: VecDeque<String>,
    dep_picked: Vec<(String, Picked)>,
}

/// Everything the host knows before any request is made.
#[derive(Debug, Clone, Deserialize)]
pub struct Inputs {
    /// The server's `TYPE`, as this project spells it.
    pub loader: String,
    /// The resolved Minecraft version.
    #[serde(rename = "gameVersion")]
    pub game_version: String,
    /// `MODRINTH_PROJECTS` verbatim.
    #[serde(default)]
    pub projects: String,
    /// `EXCLUDED_IDS` verbatim.
    #[serde(default)]
    pub excluded: String,
    /// The `mods` map already in `.homerun-loader.json`.
    #[serde(default)]
    pub existing: BTreeMap<String, ModRecord>,
    /// `modpackFiles` from the same marker.
    #[serde(default, rename = "modpackFiles")]
    pub modpack_files: Vec<String>,
    /// Project ids a modpack already provides, so they are never re-installed.
    #[serde(default, rename = "modpackProjects")]
    pub modpack_projects: Vec<String>,
    /// Jar filenames currently in the mod directory.
    #[serde(default)]
    pub present: Vec<String>,
}

// ---------------------------------------------------------------------------
// Loader-shaped answers
// ---------------------------------------------------------------------------

/// `plugins` for the Bukkit family, `mods` for everything else.
pub fn sub_dir(loader: &str) -> &'static str {
    match loader {
        "paper" | "spigot" | "bukkit" => "plugins",
        _ => "mods",
    }
}

/// The loader facet Modrinth publishes under.
///
/// Spigot and Bukkit plugins are published as Paper's, and the mapping is kept
/// even though this crate cannot host either — the facet is still the right
/// one for a Paper server, and dropping the arms would silently change what a
/// Paper server resolves if the names were ever passed through.
pub fn modrinth_facet(loader: &str) -> &str {
    match loader {
        "spigot" | "bukkit" => "paper",
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Driving
// ---------------------------------------------------------------------------

/// Start resolving. Returns the first batch of requests, or [`Progress::Done`]
/// when there is nothing to install and only a sweep to do.
pub fn begin(inputs: Inputs) -> Progress {
    let excluded: BTreeSet<String> = split_list(&inputs.excluded)
        .into_iter()
        .map(|s| s.to_ascii_lowercase())
        .collect();

    // `slug` is lowercased and a pinned version id is not: one is a name we
    // compare, the other is an opaque identifier we echo.
    let entries: Vec<(String, String)> = split_list(&inputs.projects)
        .into_iter()
        .map(|entry| match entry.find(':') {
            Some(i) => (
                entry[..i].trim().to_ascii_lowercase(),
                entry[i + 1..].trim().to_string(),
            ),
            None => (entry.trim().to_ascii_lowercase(), String::new()),
        })
        .filter(|(slug, _)| !slug.is_empty() && !excluded.contains(slug))
        .collect();

    // Scoped to what the app installed before, and that scoping is the whole
    // safety of the sweep: a jar the player added by hand has no record and no
    // modpack claim, so it is never a candidate for deletion.
    let managed_before: BTreeSet<String> = inputs
        .existing
        .values()
        .map(|r| basename(&r.file_path).to_string())
        .filter(|f| !f.is_empty())
        .chain(inputs.modpack_files.iter().cloned())
        .collect();

    let mut session = Session {
        loader: inputs.loader.clone(),
        facet: modrinth_facet(&inputs.loader).to_string(),
        sub_dir: sub_dir(&inputs.loader).to_string(),
        game_version: inputs.game_version,
        entries,
        existing: inputs.existing,
        managed_before,
        present: inputs.present.into_iter().collect(),
        preserved: inputs.modpack_files.into_iter().collect(),

        phase: Phase::ResolvingTops,
        waiting: BTreeMap::new(),
        next_id: 0,

        tops: Vec::new(),
        expected: BTreeSet::new(),
        installed: Vec::new(),
        failed: Vec::new(),
        records: BTreeMap::new(),
        unsupported: BTreeSet::new(),
        installed_projects: inputs.modpack_projects.iter().cloned().collect(),
        seen: BTreeSet::new(),
        dep_queue: VecDeque::new(),
        dep_picked: Vec::new(),
    };

    let steps: Vec<Step> = session
        .entries
        .clone()
        .into_iter()
        .map(|(slug, pinned)| {
            let url = if pinned.is_empty() {
                version_list_url(&slug, &session.game_version, &session.facet)
            } else {
                format!("{API}/version/{}", encode(&pinned))
            };
            session.step_json(url, Waiting::TopVersion { slug })
        })
        .collect();

    // A server with no mods listed still has a sweep to do — the last mod
    // someone removed is still sitting in `mods/`. Finishing here rather than
    // emitting an empty batch also means the host never asks Modrinth
    // anything for a server that has nothing to ask about.
    if steps.is_empty() {
        return session.finish();
    }
    session.emit(steps)
}

/// Feed back what the host did, and get the next batch — or the outcome.
pub fn advance(session: Session, replies: Vec<Reply>) -> Progress {
    match session.phase {
        Phase::ResolvingTops => session.after_top_versions(replies),
        Phase::ClassifyingFailures => session.after_classify(replies),
        Phase::FetchingSides => session.after_sides(replies),
        Phase::InstallingTops => session.after_top_downloads(replies),
        Phase::ResolvingDeps => session.after_dep_versions(replies),
        Phase::DownloadingDeps => session.after_dep_downloads(replies),
    }
}

impl Session {
    fn step_json(&mut self, url: String, waiting: Waiting) -> Step {
        let id = self.claim_id();
        self.waiting.insert(id.clone(), waiting);
        Step::Json { id, url }
    }

    fn step_download(&mut self, picked: &Picked, waiting: Waiting) -> Step {
        let id = self.claim_id();
        self.waiting.insert(id.clone(), waiting);
        Step::Download {
            id,
            url: picked.file_url.clone(),
            filename: picked.filename.clone(),
        }
    }

    fn claim_id(&mut self) -> String {
        self.next_id += 1;
        format!("s{}", self.next_id)
    }

    /// Pair each reply with what it was waiting on, dropping any the host
    /// invented. Order follows the replies, so a host may answer out of order.
    fn take(&mut self, replies: &[Reply]) -> Vec<(Waiting, Reply)> {
        let paired = replies
            .iter()
            .filter_map(|r| self.waiting.get(&r.id).cloned().map(|w| (w, r.clone())))
            .collect();
        self.waiting.clear();
        paired
    }

    fn emit(self, steps: Vec<Step>) -> Progress {
        Progress::Steps { steps, state: self }
    }

    // --- phase 1: resolve every mod the user listed --------------------

    fn after_top_versions(mut self, replies: Vec<Reply>) -> Progress {
        let mut classify = Vec::new();
        for (waiting, reply) in self.take(&replies) {
            let Waiting::TopVersion { slug } = waiting else {
                continue;
            };

            if reply.failed() {
                // A request that errored is not the same as a project with no
                // matching build: the desktop calls that `download_failed` and
                // keeps whatever was installed before rather than deleting a
                // working mod over a transient Modrinth error.
                self.failed.push(Failed {
                    slug: slug.clone(),
                    reason: Failed::DOWNLOAD_FAILED.into(),
                });
                self.preserve(&slug);
                continue;
            }

            match reply.json.as_ref().and_then(pick) {
                Some(picked) => self.tops.push((slug, picked)),
                None => {
                    // Nothing fits. Which of the two reasons it is takes
                    // another request — "this mod has no build for your
                    // version" and "this mod has never been released" are
                    // different things to be told.
                    self.preserve(&slug);
                    classify.push(slug);
                }
            }
        }

        if !classify.is_empty() {
            self.phase = Phase::ClassifyingFailures;
            let steps: Vec<Step> = classify
                .into_iter()
                .map(|slug| {
                    let url = format!("{API}/project/{}/version", encode(&slug));
                    self.step_json(url, Waiting::TopClassify { slug })
                })
                .collect();
            return self.emit(steps);
        }
        self.start_sides()
    }

    fn after_classify(mut self, replies: Vec<Reply>) -> Progress {
        for (waiting, reply) in self.take(&replies) {
            let Waiting::TopClassify { slug } = waiting else {
                continue;
            };
            // Any version at all means it exists and does not fit; none means
            // it has never shipped. A failed lookup reads as the former,
            // matching the desktop's `catch`.
            let has_any = reply
                .json
                .as_ref()
                .and_then(|j| j.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(true);
            self.failed.push(Failed {
                slug,
                reason: if has_any {
                    Failed::INCOMPATIBLE.into()
                } else {
                    Failed::NO_RELEASE.into()
                },
            });
        }
        self.start_sides()
    }

    // --- phase 2: which of them are client-only ------------------------

    fn start_sides(mut self) -> Progress {
        if self.tops.is_empty() {
            return self.finish();
        }
        self.phase = Phase::FetchingSides;

        let ids: Vec<String> = self
            .tops
            .iter()
            .map(|(_, p)| p.project_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        let steps: Vec<Step> = ids
            .chunks(IDS_PER_CALL)
            .map(|chunk| {
                let list = json_array(chunk);
                let url = format!("{API}/projects?ids={}", encode(&list));
                self.step_json(url, Waiting::Sides)
            })
            .collect();
        self.emit(steps)
    }

    fn after_sides(mut self, replies: Vec<Reply>) -> Progress {
        // **Fail-open.** A chunk that errored contributes nothing, so its mods
        // are installed as-is. Without the side data, excluding a client-only
        // mod could strip a hard dependency of one we keep — the desktop draws
        // the same line for the same reason.
        for (_, reply) in self.take(&replies) {
            let Some(projects) = reply.json.as_ref().and_then(|j| j.as_array()) else {
                continue;
            };
            for project in projects {
                let id = project.get("id").and_then(|v| v.as_str());
                let side = project.get("server_side").and_then(|v| v.as_str());
                if let (Some(id), Some("unsupported")) = (id, side) {
                    self.unsupported.insert(id.to_string());
                }
            }
        }
        self.install_tops()
    }

    // --- phase 2b: install what is left --------------------------------

    fn install_tops(mut self) -> Progress {
        self.phase = Phase::InstallingTops;
        let mut steps = Vec::new();

        for (slug, picked) in self.tops.clone() {
            // Silently, and not as a failure: a client-only mod on a dedicated
            // server crashes it, and the player did nothing wrong by adding a
            // mod whose page does not say so.
            if self.unsupported.contains(&picked.project_id) {
                continue;
            }
            match self.install(&slug, &picked) {
                Some(step) => steps.push(step),
                None => self.note_installed(&slug, &picked),
            }
        }

        if steps.is_empty() {
            return self.queue_deps();
        }
        self.emit(steps)
    }

    fn after_top_downloads(mut self, replies: Vec<Reply>) -> Progress {
        for (waiting, reply) in self.take(&replies) {
            let Waiting::TopDownload { slug, picked } = waiting else {
                continue;
            };
            if reply.failed() {
                self.failed.push(Failed {
                    slug: slug.clone(),
                    reason: Failed::DOWNLOAD_FAILED.into(),
                });
                self.preserve(&slug);
                continue;
            }
            self.note_installed(&slug, &picked);
        }
        self.queue_deps()
    }

    // --- phase 3: everything the installed mods require ----------------

    fn queue_deps(mut self) -> Progress {
        // Seeded with what is already installed *and* whatever a modpack
        // provides, so a pack's own mods are never fetched a second time under
        // a `dep:` key — which would leave two copies of one mod in `mods/`.
        if self.seen.is_empty() {
            self.seen = self.installed_projects.clone();
        }

        let mut steps = Vec::new();
        while let Some(pid) = self.dep_queue.pop_front() {
            if !self.seen.insert(pid.clone()) {
                continue;
            }
            let url = version_list_url(&pid, &self.game_version, &self.facet);
            steps.push(self.step_json(url, Waiting::DepVersion { project_id: pid }));
        }

        if steps.is_empty() {
            return self.finish();
        }
        self.phase = Phase::ResolvingDeps;
        self.emit(steps)
    }

    fn after_dep_versions(mut self, replies: Vec<Reply>) -> Progress {
        self.dep_picked.clear();
        for (waiting, reply) in self.take(&replies) {
            let Waiting::DepVersion { project_id } = waiting else {
                continue;
            };
            // A dependency that cannot be resolved is skipped, not fatal. The
            // loader may still be satisfied — the pack may ship it, or the mod
            // may tolerate its absence — and failing the whole install over
            // one transitive library would be worse than trying.
            if reply.failed() {
                continue;
            }
            if let Some(picked) = reply.json.as_ref().and_then(pick) {
                self.dep_picked.push((project_id, picked));
            }
        }

        let mut steps = Vec::new();
        for (project_id, picked) in self.dep_picked.clone() {
            // Dependencies are installed **regardless of their own
            // `server_side`**: a hard dependency has to be present for loader
            // resolution even when it is a client-only library. `chipped`
            // needs `athena`, which Modrinth marks unsupported.
            match self.install(&dep_key(&project_id), &picked) {
                Some(step) => steps.push(step),
                None => self.note_dep_installed(&project_id, &picked),
            }
        }

        if steps.is_empty() {
            return self.queue_deps();
        }
        self.phase = Phase::DownloadingDeps;
        self.emit(steps)
    }

    fn after_dep_downloads(mut self, replies: Vec<Reply>) -> Progress {
        for (waiting, reply) in self.take(&replies) {
            let Waiting::DepDownload { project_id, picked } = waiting else {
                continue;
            };
            if reply.failed() {
                continue;
            }
            self.note_dep_installed(&project_id, &picked);
        }
        self.queue_deps()
    }

    // --- bookkeeping ---------------------------------------------------

    /// Claim [picked]'s filename, and return a download step unless the exact
    /// version is already on disk.
    ///
    /// The name is claimed either way — a file that is kept is not stale, and
    /// forgetting to say so would sweep it.
    fn install(&mut self, key: &str, picked: &Picked) -> Option<Step> {
        self.expected.insert(picked.filename.clone());

        let up_to_date = self
            .existing
            .get(key)
            .is_some_and(|r| r.version_id == picked.version_id)
            && self.present.contains(&picked.filename);
        if up_to_date {
            return None;
        }

        let waiting = match key.strip_prefix(DEP_PREFIX) {
            Some(pid) => Waiting::DepDownload {
                project_id: pid.to_string(),
                picked: picked.clone(),
            },
            None => Waiting::TopDownload {
                slug: key.to_string(),
                picked: picked.clone(),
            },
        };
        Some(self.step_download(picked, waiting))
    }

    fn note_installed(&mut self, slug: &str, picked: &Picked) {
        self.installed.push(slug.to_string());
        self.installed_projects.insert(picked.project_id.clone());
        self.seen.insert(picked.project_id.clone());
        self.record(slug, picked);
        for dep in &picked.deps {
            self.dep_queue.push_back(dep.clone());
        }
    }

    fn note_dep_installed(&mut self, project_id: &str, picked: &Picked) {
        self.installed_projects.insert(project_id.to_string());
        self.record(&dep_key(project_id), picked);
        for dep in &picked.deps {
            self.dep_queue.push_back(dep.clone());
        }
    }

    fn record(&mut self, key: &str, picked: &Picked) {
        self.records.insert(
            key.to_string(),
            ModRecord {
                version_id: picked.version_id.clone(),
                mc_version: self.game_version.clone(),
                // Our own loader name, not the Modrinth facet: this records
                // what the server is, not where the file came from.
                loader: self.loader.clone(),
                file_path: format!("{}/{}", self.sub_dir, picked.filename),
            },
        );
    }

    /// Keep a previously-installed mod that failed to resolve this run.
    ///
    /// A transient Modrinth error or a temporary incompatibility must not
    /// delete a mod that works. Keeping the record *and* claiming its filename
    /// shields it from the sweep.
    fn preserve(&mut self, slug: &str) {
        let Some(record) = self.existing.get(slug).cloned() else {
            return;
        };
        let name = basename(&record.file_path).to_string();
        if !name.is_empty() {
            self.expected.insert(name);
        }
        self.records.insert(slug.to_string(), record);
    }

    fn finish(self) -> Progress {
        let remove = sweep(
            &self.present,
            &self.expected,
            &self.preserved,
            &self.managed_before,
        );
        Progress::Done {
            outcome: Outcome {
                installed: self.installed,
                failed: self.failed,
                remove,
                records: self.records,
                sub_dir: self.sub_dir,
            },
        }
    }
}

/// Which jars in the mod directory should go.
///
/// Four conditions, and the third is the one that matters: **only files
/// the app installed before are candidates**. A jar the player dropped in by
/// hand has no record naming it and no modpack claiming it, so it is not
/// managed and is never touched. Getting this wrong deletes somebody's mods.
pub fn sweep(
    present: &BTreeSet<String>,
    expected: &BTreeSet<String>,
    preserved: &BTreeSet<String>,
    managed_before: &BTreeSet<String>,
) -> Vec<String> {
    present
        .iter()
        .filter(|f| f.to_ascii_lowercase().ends_with(".jar"))
        .filter(|f| !expected.contains(*f))
        .filter(|f| !preserved.contains(*f))
        .filter(|f| managed_before.contains(*f))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Reading Modrinth
// ---------------------------------------------------------------------------

/// The one version to install, from either a version object or a version list.
///
/// **Newest `release`, else newest `beta`, else the newest of anything.** Some
/// mods never publish a stable build for a given Minecraft version — Geyser is
/// beta-only, C2ME alpha-only — and filtering to `release` resolved them to
/// nothing and silently installed nothing at all. Modrinth returns
/// newest-first, so the first match of a type is the newest of it.
fn pick(json: &Value) -> Option<Picked> {
    let version = match json {
        Value::Array(versions) => versions
            .iter()
            .find(|v| version_type(v) == Some("release"))
            .or_else(|| versions.iter().find(|v| version_type(v) == Some("beta")))
            .or_else(|| versions.first())?,
        object => object,
    };

    let files = version.get("files")?.as_array()?;
    let primary = files
        .iter()
        .find(|f| f.get("primary").and_then(Value::as_bool) == Some(true))
        .or_else(|| files.first())?;

    Some(Picked {
        project_id: version.get("project_id")?.as_str()?.to_string(),
        version_id: version.get("id")?.as_str()?.to_string(),
        file_url: primary.get("url")?.as_str()?.to_string(),
        filename: primary.get("filename")?.as_str()?.to_string(),
        deps: version
            .get("dependencies")
            .and_then(Value::as_array)
            .map(|deps| {
                deps.iter()
                    .filter(|d| {
                        d.get("dependency_type").and_then(Value::as_str) == Some("required")
                    })
                    .filter_map(|d| d.get("project_id").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
    })
}

fn version_type(version: &Value) -> Option<&str> {
    version.get("version_type").and_then(Value::as_str)
}

fn version_list_url(project: &str, game_version: &str, facet: &str) -> String {
    format!(
        "{API}/project/{}/version?game_versions={}&loaders={}",
        encode(project),
        encode(&json_array(std::slice::from_ref(&game_version.to_string()))),
        encode(&json_array(std::slice::from_ref(&facet.to_string()))),
    )
}

// ---------------------------------------------------------------------------
// Small shared shapes
// ---------------------------------------------------------------------------

/// Auto-pulled dependencies are recorded under their project id rather than a
/// slug, because nothing asked for them by name and two mods can require the
/// same one.
const DEP_PREFIX: &str = "dep:";

fn dep_key(project_id: &str) -> String {
    format!("{DEP_PREFIX}{project_id}")
}

/// Newline- or comma-separated, trimmed, blanks dropped.
///
/// Public because [`super::crossplay`] merges into one of these strings and
/// must split it exactly as this module later will — two splitters that
/// disagree would let a duplicate slug through and install one plugin twice.
pub fn split_list(raw: &str) -> Vec<String> {
    raw.split(['\n', ','])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// `["a","b"]`, the shape Modrinth's array parameters take.
fn json_array(items: &[String]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|s| serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\"")))
        .collect();
    format!("[{}]", quoted.join(","))
}

/// `encodeURIComponent`, so a URL built here is byte-identical to the
/// desktop's. Its unreserved set is wider than percent-encoding's default and
/// the difference shows up in real slugs.
///
/// Shared with [`super::loader`] rather than copied: it builds one URL out of a
/// Minecraft version, and two encoders that must agree is exactly the drift
/// this crate exists to avoid.
pub(super) fn encode(raw: &str) -> String {
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
