//! Getting a game's server onto this machine.
//!
//! # We are not a mirror
//!
//! Every byte here comes from the vendor. Homerun downloads a game's server
//! onto the player's own machine, after that player has accepted the game's
//! terms; it does not host, repackage or redistribute one. `steamcmd` is
//! fetched from Valve at first use for exactly the same reason — shipping a
//! copy of Valve's client inside our installer would be redistributing it.
//!
//! # Anonymous only, and never accepting terms for anyone
//!
//! `steamcmd` is driven with `+login anonymous` and nothing else. A game
//! whose dedicated server needs an account that *owns* it is out of scope.
//!
//! And if `steamcmd` or a vendor's installer asks someone to agree to
//! something, this module **stops**. It does not answer, it does not pass
//! `-accept-eula`, and it does not retry. [`prompt_detected`] is what makes
//! that a rule rather than an intention: the output is scanned for the shapes
//! an agreement prompt takes, and finding one ends the fetch with a message
//! telling the person that a human has to look. Accepting terms on someone
//! else's behalf is not a thing this program does.
//!
//! # Why the archive is unpacked carefully even though it is pinned
//!
//! A direct download is pinned by sha256 in a descriptor that arrived inside
//! a signed host build, so its *bytes* are exactly the bytes someone vetted.
//! That says nothing about the *paths inside it*. A vendor archive nobody has
//! audited entry-by-entry can still contain `../../windows/system32/...`, and
//! a pinned digest would be a pinned digest of a malicious layout.
//!
//! So [`extract_zip`] refuses absolute paths, drive letters, and any entry
//! that climbs out of the destination — the same rule `scripts/ui-bundle.js`
//! applies to a UI bundle, arrived at the same way. What is deliberately
//! *not* imposed is an entry-count or size ceiling: a game runtime genuinely
//! is tens of thousands of files and many gigabytes, and a ceiling tuned for
//! a UI bundle would refuse every real game.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use homerun_core::engine::fetch::{Plan, Present};
use sha2::{Digest, Sha256};

/// Where a fetch has got to.
///
/// The phases are the contract's (`steamcmd|download|verify|extract`), so a
/// host can render them without translating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// A phase began, or has something to say.
    Note {
        phase: &'static str,
        message: String,
    },
    /// Bytes moved. `total` is `None` when the server did not say how big it
    /// is, which is common and not an error.
    Bytes {
        phase: &'static str,
        received: u64,
        total: Option<u64>,
    },
    /// A vendor's downloader is waiting for the person to sign in: open `url`
    /// and, if there is one, enter `code`. Sent again whenever either
    /// changes. Nothing here answers it; the person does, in their browser.
    SignIn { url: String, code: Option<String> },
}

/// What a completed fetch left on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub dir: PathBuf,
    /// Recorded beside the runtime so the next [`Plan`] can skip the work.
    pub build_id: String,
}

/// The file a runtime directory keeps to say what is in it.
const STAMP: &str = ".homerun-build";

/// Read what a runtime directory says it holds.
///
/// A directory with no stamp is empty as far as this is concerned, even if
/// files are present: a half-finished download that was interrupted has files
/// and is not a runtime.
pub fn present(dir: &Path) -> Present {
    Present {
        build_id: fs::read_to_string(dir.join(STAMP))
            .ok()
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty()),
        // Nothing here can see a *reason* to doubt the install -- that is a
        // fact about the last launch, or about a person asking for a repair,
        // and it belongs to whoever knows it. A caller that has one sets this
        // on the value this returns.
        suspect: false,
    }
}

/// Everything a fetch needs that is not in the plan.
pub struct Context<'a> {
    /// Where `steamcmd` itself is cached. Not the game's directory: one copy
    /// serves every steam game on the machine.
    pub tools_dir: PathBuf,
    pub on_progress: &'a dyn Fn(Progress),
    /// Answered often, and a `true` ends the fetch at the next boundary.
    pub cancelled: &'a dyn Fn() -> bool,
}

/// Carry out a plan.
pub fn fetch(plan: &Plan, ctx: &Context) -> Result<Fetched, String> {
    match plan {
        Plan::AlreadyPresent { dir, build_id } => Ok(Fetched {
            dir: PathBuf::from(dir),
            build_id: build_id.clone(),
        }),
        Plan::Direct {
            dir,
            url,
            sha256,
            size,
            extract,
            strip_components,
        } => direct(
            Path::new(dir),
            url,
            sha256,
            *size,
            *extract,
            *strip_components,
            ctx,
        ),
        Plan::Vendor {
            dir,
            url,
            version,
            size,
            strip_components,
            record,
        } => vendor(
            Path::new(dir),
            url,
            version,
            *size,
            *strip_components,
            Path::new(record),
            ctx,
        ),
        Plan::SteamCmd {
            dir,
            app_id,
            build_id,
            verify,
        } => steamcmd(Path::new(dir), *app_id, build_id.as_deref(), *verify, ctx),
        Plan::Tool {
            dir,
            tool,
            args,
            version_args,
            sign_in,
            present_build_id,
        } => vendor_tool(
            Path::new(dir),
            tool,
            args,
            version_args,
            sign_in.as_ref(),
            present_build_id.as_deref(),
            ctx,
        ),
    }
}

// ─── a vendor's own downloader ──────────────────────────────────────────────
//
// For a game whose server files only its owner's account can fetch. The
// downloader is pinned and verified like any direct download, then run with
// the person's own sign-in: it prints a verification address and code, the
// host shows them, and the person approves it in their own browser. Homerun
// never sees a password and never answers the prompt.
//
// The credentials the downloader keeps live beside `steamcmd` in the tools
// directory, one file per game -- never in a server folder, so never in a
// backup, and never anywhere Homerun's servers read. This module does not
// open that file; it only tells the downloader where it is.
//
// What the downloader fetches cannot be pinned: the vendor serves only its
// current build, and a game whose client and server must match exactly is
// unplayable on anything older. So the build a runtime holds is the version
// the downloader reported, and every fetch asks first. If asking fails (no
// network) and something is installed, what is installed is used.

/// The placeholders a downloader's arguments may use, and nothing else.
const OUTPUT: &str = "{output}";
const CREDENTIALS: &str = "{credentials}";

fn vendor_tool(
    dir: &Path,
    tool: &homerun_core::engine::descriptor::Tool,
    args: &[String],
    version_args: &[String],
    sign_in: Option<&homerun_core::engine::descriptor::SignIn>,
    present_build_id: Option<&str>,
    ctx: &Context,
) -> Result<Fetched, String> {
    let game = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "game".into());

    // The downloader itself: pinned, verified and unpacked once per pin.
    let tool_dir = ctx.tools_dir.join("tools").join(format!(
        "{game}-{}",
        &tool.sha256[..tool.sha256.len().min(12)]
    ));
    if present(&tool_dir).build_id.as_deref()
        != Some(homerun_core::engine::fetch::direct_build_id(&tool.sha256).as_str())
        || !crate::platform::executable(&tool_dir, &tool.exe).is_file()
    {
        (ctx.on_progress)(Progress::Note {
            phase: "download",
            message: "getting the game's downloader".to_string(),
        });
        direct(
            &tool_dir,
            &tool.url,
            &tool.sha256,
            None,
            tool.extract,
            0,
            ctx,
        )?;
    }
    let program = crate::platform::executable(&tool_dir, &tool.exe);
    if !program.is_file() {
        return Err("the game's downloader did not contain the program it should.".to_string());
    }

    let credentials_dir = ctx.tools_dir.join("credentials");
    fs::create_dir_all(&credentials_dir).map_err(|_| cannot_write(&credentials_dir))?;
    let credentials = credentials_dir.join(format!("{game}.json"));
    fs::create_dir_all(dir).map_err(|_| cannot_write(dir))?;
    let output = dir.join(".download.zip");
    let fill = |list: &[String]| -> Vec<String> {
        list.iter()
            .map(|a| {
                a.replace(OUTPUT, &output.to_string_lossy())
                    .replace(CREDENTIALS, &credentials.to_string_lossy())
            })
            .collect()
    };

    // Ask which build is current. It may ask the person to sign in first.
    let mut version = None;
    if !version_args.is_empty() {
        match run_tool(&program, &fill(version_args), &tool_dir, sign_in, ctx) {
            Ok(lines) => {
                version = lines
                    .iter()
                    .rev()
                    .map(|l| l.trim())
                    .find(|l| !l.is_empty())
                    .map(str::to_string);
            }
            Err(e) if (ctx.cancelled)() => return Err(e),
            Err(e) => {
                if let Some(installed) = present_build_id {
                    (ctx.on_progress)(Progress::Note {
                        phase: "download",
                        message: format!(
                            "could not check for a newer version ({e}); using the one installed"
                        ),
                    });
                    return Ok(Fetched {
                        dir: dir.to_path_buf(),
                        build_id: installed.to_string(),
                    });
                }
                return Err(e);
            }
        }
        if let (Some(v), Some(installed)) = (&version, present_build_id) {
            if v == installed {
                return Ok(Fetched {
                    dir: dir.to_path_buf(),
                    build_id: installed.to_string(),
                });
            }
        }
    }

    let _ = fs::remove_file(&output);
    run_tool(&program, &fill(args), &tool_dir, sign_in, ctx)?;
    if !output.is_file() {
        return Err(
            "the game's downloader finished without leaving the game's files where Homerun \
             asked for them."
                .to_string(),
        );
    }
    let build_id = match version {
        Some(v) => v,
        None => homerun_core::engine::fetch::direct_build_id(&digest_of(&output)?),
    };
    (ctx.on_progress)(Progress::Note {
        phase: "extract",
        message: "unpacking".to_string(),
    });
    extract_zip(&output, dir, ctx)?;
    let _ = fs::remove_file(&output);
    stamp(dir, &build_id)?;
    Ok(Fetched {
        dir: dir.to_path_buf(),
        build_id,
    })
}

/// Run a vendor's downloader to completion, reporting its output and any
/// sign-in it asks for. Returns its output lines; a non-zero exit is an error.
fn run_tool(
    program: &Path,
    args: &[String],
    cwd: &Path,
    sign_in: Option<&homerun_core::engine::descriptor::SignIn>,
    ctx: &Context,
) -> Result<Vec<String>, String> {
    use std::process::{Command, Stdio};

    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        // Nothing is ever typed into it. A downloader that wants an answer on
        // its console gets end-of-file, like steamcmd does.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| "Homerun could not run the game's downloader.".to_string())?;

    let (tx, rx) = std::sync::mpsc::sync_channel(256);
    for reader in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            crate::process_engine::read_lines_lossy(reader, |line| tx.send(line).is_ok());
        });
    }
    drop(tx);

    let mut lines = Vec::new();
    let mut url: Option<String> = None;
    let mut code: Option<String> = None;
    loop {
        if (ctx.cancelled)() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(cancelled());
        }
        let line = match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(line) => line,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                continue;
            }
        };
        // The same rule as steamcmd: an agreement prompt ends the run.
        if prompt_detected(&line) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(
                "the game's downloader asked for agreement to terms, which a person has to \
                 give themselves. Run the vendor's downloader yourself once, then try again."
                    .to_string(),
            );
        }
        if let Some(markers) = sign_in {
            let (found_url, found_code) = sign_in_parts(&line, markers);
            let found_url = found_url.filter(|new| replaces_sign_in_url(url.as_deref(), new));
            let changed = found_url.is_some() || (found_code.is_some() && found_code != code);
            if found_url.is_some() {
                url = found_url;
            }
            if found_code.is_some() {
                code = found_code;
            }
            if changed {
                if let Some(u) = &url {
                    (ctx.on_progress)(Progress::SignIn {
                        url: u.clone(),
                        code: code.clone(),
                    });
                }
            }
        }
        (ctx.on_progress)(Progress::Note {
            phase: "download",
            message: line.clone(),
        });
        if lines.len() >= 200 {
            lines.remove(0);
        }
        lines.push(line);
    }
    let status = child
        .wait()
        .map_err(|_| "the game's downloader could not be waited for.".to_string())?;
    if !status.success() {
        return Err(format!(
            "the game's downloader stopped with an error{}",
            lines
                .iter()
                .rev()
                .find(|l| !l.trim().is_empty())
                .map(|l| format!(": {}", l.trim()))
                .unwrap_or_else(|| ".".to_string())
        ));
    }
    Ok(lines)
}

/// Whether a newly seen sign-in address should replace the one already shown.
///
/// Downloaders and servers print the address twice: once with the code
/// already in it (`.../verify?user_code=...`) and once bare, for typing the
/// code by hand. The one with the code in it is the better thing to open, so
/// a bare address never replaces a longer one it is the start of.
pub fn replaces_sign_in_url(current: Option<&str>, new: &str) -> bool {
    match current {
        None => true,
        Some(current) => current != new && !current.starts_with(new),
    }
}

/// A line with terminal escape sequences (colours, resets) removed.
///
/// Hytale's server ends every log line with a colour reset even when told the
/// console is not a terminal, and an address with `\x1b[m` stuck to its end
/// is not one a browser can open.
pub fn without_escapes(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // CSI: ESC [ parameters... final byte in '@'..='~'.
        if chars.peek() == Some(&'[') {
            chars.next();
            for f in chars.by_ref() {
                if ('@'..='~').contains(&f) {
                    break;
                }
            }
        } else {
            // Any other escape: drop the single character that follows.
            chars.next();
        }
    }
    out
}

/// The verification address and code on one line of a downloader's output.
pub fn sign_in_parts(
    line: &str,
    markers: &homerun_core::engine::descriptor::SignIn,
) -> (Option<String>, Option<String>) {
    let line = without_escapes(line);
    let line = line.as_str();
    let url = (!markers.url.is_empty() && line.contains(&markers.url))
        .then(|| {
            line.split_whitespace()
                .find(|word| word.starts_with("https://") && word.contains(&markers.url))
                .map(str::to_string)
        })
        .flatten();
    let code = (!markers.code.is_empty())
        .then(|| {
            line.find(&markers.code)
                .map(|at| line[at + markers.code.len()..].trim().to_string())
                .filter(|c| !c.is_empty() && !c.contains(char::is_whitespace))
        })
        .flatten();
    (url, code)
}

// ─── a pinned URL ───────────────────────────────────────────────────────────

fn direct(
    dir: &Path,
    url: &str,
    expected: &str,
    size: Option<u64>,
    extract: homerun_core::engine::descriptor::Extract,
    strip_components: u32,
    ctx: &Context,
) -> Result<Fetched, String> {
    use homerun_core::engine::descriptor::Extract;

    fs::create_dir_all(dir).map_err(|_| cannot_write(dir))?;

    // A partial file, so an interrupted download resumes instead of starting
    // a multi-gigabyte transfer again. It is never the finished artefact: the
    // rename happens only after the digest matches.
    let part = dir.join(".download.part");
    download(url, &part, size, None, ctx)?;

    (ctx.on_progress)(Progress::Note {
        phase: "verify",
        message: "checking the download".to_string(),
    });
    let actual = digest_of(&part)?;
    if !actual.eq_ignore_ascii_case(expected) {
        // Delete it. Keeping a file that failed its digest invites a later
        // run to resume *into* it and fail again for ever -- the desktop's
        // Pumpkin runtime learned this as "delete-on-corrupt".
        let _ = fs::remove_file(&part);
        return Err(
            "the download did not arrive intact, so Homerun has not used it. \
             Trying again usually fixes this."
                .to_string(),
        );
    }

    match extract {
        Extract::Zip => {
            (ctx.on_progress)(Progress::Note {
                phase: "extract",
                message: "unpacking".to_string(),
            });
            extract_zip_stripped(&part, dir, strip_components, ctx)?;
            let _ = fs::remove_file(&part);
        }
        Extract::None => {
            // Not an archive: it is the program. Keep the vendor's own file
            // name from the URL so the descriptor's `exe` can name it.
            let name = url
                .rsplit('/')
                .next()
                .filter(|n| !n.is_empty() && !n.contains('?'))
                .unwrap_or("server");
            let target = dir.join(name);
            fs::rename(&part, &target).map_err(|_| cannot_write(dir))?;
            crate::platform::make_executable(&target)?;
        }
    }

    let build_id = homerun_core::engine::fetch::direct_build_id(expected);
    stamp(dir, &build_id)?;
    Ok(Fetched {
        dir: dir.to_path_buf(),
        build_id,
    })
}

// ─── the vendor's own site, in a version the host chose ─────────────────────

/// The per-machine record of what each version's download hashed to the
/// first time, keyed by version.
type VendorHashes = std::collections::BTreeMap<String, String>;

/// Fetch one version of a runtime from the vendor's own site.
///
/// No digest can be pinned for a version released after the descriptor was
/// signed, so this checks what can be checked and remembers the rest:
///
///  - the address is the descriptor's, over HTTPS, and a redirect to any
///    other origin is refused rather than followed;
///  - the bytes that arrive are as many as the server said it would send,
///    and as many as `size` says when the descriptor gives one;
///  - every archive member is read to its end so its CRC is checked, and
///    nothing is unpacked outside the version's directory;
///  - the sha256 of the first download of each version is recorded in
///    `record`, and every later download of that version must match it. A
///    vendor that changed the file behind a version is refused before
///    anything is unpacked, let alone run.
///
/// The record is written only after the download has been unpacked, so a
/// download that failed any other check is never what later ones are held
/// to.
fn vendor(
    dir: &Path,
    url: &str,
    version: &str,
    size: Option<u64>,
    strip_components: u32,
    record: &Path,
    ctx: &Context,
) -> Result<Fetched, String> {
    // The plan already refused these; a plan can also arrive as JSON from a
    // host, so they are refused again where the bytes are fetched.
    homerun_core::engine::fetch::check_runtime_version(version).map_err(|e| e.to_string())?;
    if !homerun_core::engine::fetch::vendor_scheme_allowed(url) {
        return Err(
            "this game's download address is not a secure (https) one, so Homerun \
             will not download from it."
                .to_string(),
        );
    }
    let origin = reqwest::Url::parse(url)
        .map_err(|_| "this game's download address is not one Homerun can read.".to_string())?;

    // Read before anything is fetched: a record that cannot be read is
    // refused rather than treated as empty, which would forget every
    // version this machine has already seen.
    let mut known = read_vendor_hashes(record)?;

    fs::create_dir_all(dir).map_err(|_| cannot_write(dir))?;

    // Never resumed. A resumed transfer would be hashed as one file made of
    // two responses, and the first download of a version is the one every
    // later download is held to.
    let part = dir.join(".download.part");
    let _ = fs::remove_file(&part);
    download(url, &part, None, Some(&origin), ctx)?;

    if let Some(size) = size {
        let arrived = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        if arrived != size {
            let _ = fs::remove_file(&part);
            return Err(format!(
                "the download from {} was not the size this game's server should be, \
                 so Homerun has not used it. Trying again usually fixes this.",
                host_of(url)
            ));
        }
    }

    (ctx.on_progress)(Progress::Note {
        phase: "verify",
        message: "checking the download".to_string(),
    });
    let actual = digest_of(&part)?;
    let first = match known.get(version) {
        Some(expected) if !expected.eq_ignore_ascii_case(&actual) => {
            let _ = fs::remove_file(&part);
            return Err(format!(
                "the file {} serves for version {version} is not the one this computer \
                 downloaded for that version before. The vendor's file for that version \
                 changed, so Homerun has not unpacked or run it.",
                host_of(url)
            ));
        }
        Some(_) => false,
        None => true,
    };

    (ctx.on_progress)(Progress::Note {
        phase: "extract",
        message: "unpacking".to_string(),
    });
    // Nothing in this directory is a runtime until it is stamped again: an
    // unpack interrupted after this point must not look finished.
    let _ = fs::remove_file(dir.join(STAMP));
    let unpacked = extract_zip_stripped(&part, dir, strip_components, ctx);
    let _ = fs::remove_file(&part);
    unpacked?;

    if first {
        known.insert(version.to_string(), actual);
        write_vendor_hashes(record, &known)?;
    }

    let build_id = homerun_core::engine::fetch::vendor_build_id(version);
    stamp(dir, &build_id)?;
    Ok(Fetched {
        dir: dir.to_path_buf(),
        build_id,
    })
}

fn read_vendor_hashes(record: &Path) -> Result<VendorHashes, String> {
    match fs::read(record) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| {
            format!(
                "Homerun's record of this game's downloads ({}) cannot be read, so it \
                 cannot tell whether a download is the one it saw before. Nothing was \
                 downloaded.",
                record.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(VendorHashes::new()),
        Err(_) => Err(cannot_read(record)),
    }
}

/// Written beside itself and renamed into place, so an interruption leaves
/// the old record or the new one and never half of either.
fn write_vendor_hashes(record: &Path, known: &VendorHashes) -> Result<(), String> {
    if let Some(parent) = record.parent() {
        fs::create_dir_all(parent).map_err(|_| cannot_write(parent))?;
    }
    let staged = record.with_extension("json.part");
    let text = serde_json::to_vec_pretty(known).map_err(|_| cannot_write(record))?;
    fs::write(&staged, text).map_err(|_| cannot_write(record))?;
    fs::rename(&staged, record).map_err(|_| {
        let _ = fs::remove_file(&staged);
        cannot_write(record)
    })
}

/// Stream a URL to a file, resuming a `.part` that is already there.
///
/// `same_origin`, when given, is the only origin a redirect may lead to; a
/// redirect anywhere else ends the download instead of being followed. The
/// scheme is part of the origin, so an HTTPS address cannot be redirected to
/// plain HTTP either.
fn download(
    url: &str,
    part: &Path,
    expected_size: Option<u64>,
    same_origin: Option<&reqwest::Url>,
    ctx: &Context,
) -> Result<(), String> {
    let already = fs::metadata(part).map(|m| m.len()).unwrap_or(0);

    // A part that is already the full size is a download that finished and
    // was interrupted before its digest was checked. Let the caller check it.
    if let Some(size) = expected_size {
        if already == size {
            return Ok(());
        }
        if already > size {
            // Longer than it should be: not a prefix of anything. Start over.
            let _ = fs::remove_file(part);
            return download(url, part, expected_size, same_origin, ctx);
        }
    }

    // A synchronous caller still needs to cancel while the peer is silent.
    // Racing the whole transfer against cancellation drops the socket future,
    // without imposing a deadline on legitimate multi-gigabyte downloads.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "Homerun could not start a download worker.".to_string())?;
    runtime.block_on(async {
        tokio::select! {
           result = async {
        let redirects = match same_origin {
            None => reqwest::redirect::Policy::default(),
            Some(origin) => {
                let origin = origin.origin();
                reqwest::redirect::Policy::custom(move |attempt| {
                    if attempt.previous().len() >= 10 {
                        attempt.error("too many redirects")
                    } else if attempt.url().origin() == origin {
                        attempt.follow()
                    } else {
                        attempt.error("redirected off the vendor's site")
                    }
                })
            }
        };
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .redirect(redirects)
            .build()
            .map_err(|_| "Homerun could not start a download on this computer.".to_string())?;

        let mut request = client.get(url);
        if already > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={already}-"));
        }

        let mut response = request.send().await.map_err(|err| {
            if err.is_redirect() && same_origin.is_some() {
                format!(
                    "{} tried to send this download somewhere other than its own site, \
                     so Homerun has not followed it.",
                    host_of(url)
                )
            } else {
                format!("Homerun could not reach {}.", host_of(url))
            }
        })?;
        if let Some(origin) = same_origin {
            // The policy above already refused anything else; this is the
            // check that does not depend on how a redirect was followed.
            if response.url().origin() != origin.origin() {
                return Err(format!(
                    "{} tried to send this download somewhere other than its own \
                     site, so Homerun has not followed it.",
                    host_of(url)
                ));
            }
        }

        if !response.status().is_success() {
            return Err(format!(
                "{} did not have the file Homerun asked for.",
                host_of(url)
            ));
        }

        // A server that ignored the Range header sends 200 and the whole file;
        // appending to the part would then corrupt it in a way the digest catches
        // but only after the whole transfer.
        let resuming = already > 0 && response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
        let mut file = if resuming {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .open(part)
                .map_err(|_| cannot_write(part))?;
            file.seek(SeekFrom::Start(already))
                .map_err(|_| cannot_write(part))?;
            file
        } else {
            fs::File::create(part).map_err(|_| cannot_write(part))?
        };

        let mut received = if resuming { already } else { 0 };
        let started_at = received;
        // What the server said it would send. A body that ends short of it
        // is a truncated file, whatever the connection claims.
        let announced = response.content_length();
        let total = response
            .content_length()
            .map(|len| len + if resuming { already } else { 0 })
            .or(expected_size);

        let mut since_report = 0u64;
        loop {
            if (ctx.cancelled)() {
                return Err(cancelled());
            }
            let chunk = response
                .chunk().await
                .map_err(|_| format!("the download from {} was interrupted.", host_of(url)))?;
            let Some(chunk) = chunk else { break; };
            let read = chunk.len();
            file.write_all(&chunk)
                .map_err(|_| cannot_write(part))?;
            received += read as u64;
            since_report += read as u64;

            // Every few megabytes rather than every chunk: this crosses an FFI
            // boundary and then a JSON line, and a report per 512KiB of a nine
            // gigabyte download is eighteen thousand of them.
            if since_report >= 4 * 1024 * 1024 {
                since_report = 0;
                (ctx.on_progress)(Progress::Bytes {
                    phase: "download",
                    received,
                    total,
                });
            }
        }

        file.flush().map_err(|_| cannot_write(part))?;
        if announced.is_some_and(|len| received - started_at != len) {
            return Err(format!(
                "the download from {} ended before the whole file arrived. Trying \
                 again usually fixes this.",
                host_of(url)
            ));
        }
        (ctx.on_progress)(Progress::Bytes {
            phase: "download",
            received,
            total,
        });
        Ok(())
           } => result,
           _ = async { loop {
               if (ctx.cancelled)() { return; }
               tokio::time::sleep(std::time::Duration::from_millis(100)).await;
           }} => Err(cancelled()),
          }
    })
}

/// The sha256 of a file, read in one pass.
///
/// Computed after the download rather than during it, because a resumed
/// download has no running hash to continue from and a hash that was only
/// correct for un-resumed transfers would be worse than none.
pub fn digest_of(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|_| cannot_read(path))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 512 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|_| cannot_read(path))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

// ─── unpacking ──────────────────────────────────────────────────────────────

/// Unpack a zip into a directory, refusing anything that would land outside.
pub fn extract_zip(archive: &Path, into: &Path, ctx: &Context) -> Result<(), String> {
    extract_zip_stripped(archive, into, 0, ctx)
}

/// [`extract_zip`], dropping `strip` leading path components from every
/// entry first -- for an archive that nests everything under one top folder,
/// such as Terraria's `1458/`.
///
/// Every file entry is read to its last byte, including one that stripping
/// leaves with no name and is skipped: the zip library checks an entry's CRC
/// only when it reaches the end, so an entry that is not read to the end is
/// an entry that was never checked. A mismatch is a refusal, not a warning.
/// Traversal is judged on the entry's full name, before anything is dropped.
pub fn extract_zip_stripped(
    archive: &Path,
    into: &Path,
    strip: u32,
    ctx: &Context,
) -> Result<(), String> {
    let file = fs::File::open(archive).map_err(|_| cannot_read(archive))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|_| "the download is not an archive Homerun can open.".to_string())?;

    for index in 0..zip.len() {
        if (ctx.cancelled)() {
            return Err(cancelled());
        }
        let mut entry = zip
            .by_index(index)
            .map_err(|_| "the archive could not be read to the end.".to_string())?;

        // `enclosed_name` is the library's own answer to this and it returns
        // `None` for absolute paths, drive letters and anything containing
        // `..`. It is used rather than a hand-rolled check because getting
        // this wrong is silent, and because it also handles the Windows
        // spellings a Unix-only check misses.
        let Some(relative) = entry.enclosed_name() else {
            return Err(format!(
                "the archive contains a file ({}) that would be written outside \
                 this server's folder, so Homerun has not unpacked it.",
                entry.name()
            ));
        };

        // `enclosed_name` allows a `..` that stays inside, such as `a/../b`.
        // Dropping the leading `a` would turn that into `../b`, one level
        // above the directory -- where the record of this game's downloads
        // lives. So a name that is not plain components is refused whenever
        // anything is being dropped from it.
        if strip > 0
            && relative
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(format!(
                "the archive contains a file ({}) that would be written outside \
                 this server's folder, so Homerun has not unpacked it.",
                entry.name()
            ));
        }
        let stripped: PathBuf = relative.components().skip(strip as usize).collect();
        if stripped.as_os_str().is_empty() {
            // Nothing left to name it by. Still read, so it is still checked.
            if !entry.is_dir() {
                copy_checked(&mut entry, &mut std::io::sink(), &relative, into)?;
            }
            continue;
        }

        let target = into.join(stripped);
        if entry.is_dir() {
            fs::create_dir_all(&target).map_err(|_| cannot_write(&target))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|_| cannot_write(parent))?;
        }

        let mut out = fs::File::create(&target).map_err(|_| cannot_write(&target))?;
        copy_checked(&mut entry, &mut out, &relative, &target).map_err(|err| {
            // Not left behind looking like a file that arrived.
            drop(fs::remove_file(&target));
            err
        })?;

        // An archive built on Windows records no Unix mode, so a server
        // binary unpacked on Linux routinely arrives without its execute bit.
        #[cfg(unix)]
        if entry
            .unix_mode()
            .map(|mode| mode & 0o111 != 0)
            .unwrap_or(false)
        {
            let _ = crate::platform::make_executable(&target);
        }
    }
    Ok(())
}

/// Copy one archive entry to its end, telling a damaged entry (a read that
/// fails, which is how the zip library reports a CRC mismatch) apart from a
/// disk that would not take it.
fn copy_checked(
    entry: &mut impl Read,
    out: &mut impl Write,
    name: &Path,
    target: &Path,
) -> Result<(), String> {
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = entry.read(&mut buffer).map_err(|_| {
            format!(
                "the archive is damaged: {} did not match its checksum, so Homerun \
                 has not used it. Trying again usually fixes this.",
                name.display()
            )
        })?;
        if read == 0 {
            return Ok(());
        }
        out.write_all(&buffer[..read])
            .map_err(|_| cannot_write(target))?;
    }
}

// ─── steamcmd ───────────────────────────────────────────────────────────────

/// Valve's own download for the Windows client. Never mirrored by us.
const STEAMCMD_WINDOWS: &str = "https://steamcdn-a.akamaihd.net/client/installer/steamcmd.zip";

/// Where an already-installed `steamcmd` can be named, for a machine that has
/// one or a platform this cannot bootstrap on.
pub const STEAMCMD_OVERRIDE: &str = "HOMERUN_STEAMCMD";

fn steamcmd(
    dir: &Path,
    app_id: u32,
    build_id: Option<&str>,
    verify: bool,
    ctx: &Context,
) -> Result<Fetched, String> {
    let binary = steamcmd_binary(ctx)?;
    fs::create_dir_all(dir).map_err(|_| cannot_write(dir))?;

    (ctx.on_progress)(Progress::Note {
        phase: "steamcmd",
        message: if verify {
            "checking every file of this game's server".to_string()
        } else {
            "asking Steam for the server files".to_string()
        },
    });

    let args = steamcmd_args(dir, app_id, build_id, verify);
    let output = run_streaming(&binary, &args, "steamcmd", ctx)?;

    if prompt_detected(&output) {
        return Err(
            "Steam is asking someone to agree to its terms before it will download \
             this game's server. Homerun will not answer that for you — run \
             steamcmd yourself once, read what it asks, and answer it."
                .to_string(),
        );
    }

    // steamcmd's exit code is unreliable across versions; its own success
    // line is not. Both are checked, and the line is what decides.
    if !output.contains("Success! App") && !output.contains("fully installed") {
        return Err(
            "Steam did not finish downloading this game's server. Trying again \
             usually fixes this."
                .to_string(),
        );
    }

    let stamped = build_id.map(str::to_string).unwrap_or_else(|| {
        // Nothing to pin, so the stamp records only that *something* is here.
        // `engine::fetch::plan` never treats an unpinned steam runtime as
        // already present, so this is a record rather than a decision.
        format!("app{app_id}")
    });
    stamp(dir, &stamped)?;
    Ok(Fetched {
        dir: dir.to_path_buf(),
        build_id: stamped,
    })
}

/// What steamcmd is told to do, and nothing else.
///
/// Separated from the run so the argument list is testable without Valve's
/// client on the machine: every rule below is invisible in its effect and
/// expensive when it is wrong.
fn steamcmd_args(dir: &Path, app_id: u32, build_id: Option<&str>, verify: bool) -> Vec<String> {
    // `+force_install_dir` before `+login` is not stylistic: steamcmd applies
    // it to the app_update that follows, and putting it after the login makes
    // it silently install to its own default directory instead.
    let mut args = vec![
        "+force_install_dir".to_string(),
        dir.to_string_lossy().into_owned(),
        "+login".to_string(),
        "anonymous".to_string(),
        "+app_update".to_string(),
        app_id.to_string(),
    ];
    // A pinned build needs the beta branch machinery; without a pin, take
    // whatever is current, which is what a force-updating game requires
    // anyway.
    if let Some(build) = build_id {
        args.push("-beta".to_string());
        args.push(build.to_string());
    }
    // `validate` re-reads and checksums every file, which the Rust pilot
    // measured at 5,869,171,402 bytes and minutes per start on a runtime that
    // was already complete and already current. `engine::fetch` decides when
    // that is worth doing; an ordinary update is not one of those times.
    if verify {
        args.push("validate".to_string());
    }
    args.push("+quit".to_string());
    args
}

/// The `steamcmd` to drive: one a person named, one already cached, or one
/// fetched from Valve.
fn steamcmd_binary(ctx: &Context) -> Result<PathBuf, String> {
    if let Some(named) = std::env::var_os(STEAMCMD_OVERRIDE) {
        let path = PathBuf::from(named);
        if path.exists() {
            return Ok(path);
        }
        return Err(format!(
            "{STEAMCMD_OVERRIDE} names a program that is not there."
        ));
    }

    let dir = ctx.tools_dir.join("steamcmd");
    let binary = crate::platform::executable(&dir, "steamcmd");
    if binary.exists() {
        return Ok(binary);
    }

    if !cfg!(windows) {
        return Err(format!(
            "Homerun can only install Steam's downloader on Windows. Install \
             steamcmd yourself and point {STEAMCMD_OVERRIDE} at it."
        ));
    }

    (ctx.on_progress)(Progress::Note {
        phase: "steamcmd",
        message: "getting Steam's downloader".to_string(),
    });
    fs::create_dir_all(&dir).map_err(|_| cannot_write(&dir))?;

    // Deliberately not pinned by digest, unlike everything else here: Valve
    // republishes this URL in place on every client update, so a pin would
    // turn every Steam game into a failed download the day after it moved.
    // What makes that acceptable is that it is HTTPS direct from Valve and
    // it is the same file Steam's own documentation tells a person to fetch.
    let archive = dir.join("steamcmd.zip");
    download(STEAMCMD_WINDOWS, &archive, None, None, ctx)?;
    extract_zip(&archive, &dir, ctx)?;
    let _ = fs::remove_file(&archive);

    if !binary.exists() {
        return Err("Steam's downloader did not arrive in one piece.".to_string());
    }
    Ok(binary)
}

/// Run a program, reporting its output as it arrives.
fn run_streaming(
    program: &Path,
    args: &[String],
    phase: &'static str,
    ctx: &Context,
) -> Result<String, String> {
    use std::process::{Command, Stdio};

    let mut child = Command::new(program)
        .args(args)
        // stdin is closed rather than inherited. If steamcmd ever *does* want
        // an answer, it gets end-of-file and gives up, which is the behaviour
        // this module wants: see the note on prompts in the module header.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| format!("Homerun could not run {}.", program.display()))?;

    let (tx, rx) = std::sync::mpsc::sync_channel(256);
    // Lossy, and for a sharper reason here than for a game's console: a
    // Windows username that is not ASCII appears in the paths steamcmd
    // prints, in the machine's code page rather than UTF-8. Ending the pump
    // on the first such line means "Success! App" is never seen, and the
    // fetch fails -- every time, on that machine, for ever.
    fn pump(reader: impl Read + Send + 'static, tx: std::sync::mpsc::SyncSender<String>) {
        std::thread::spawn(move || {
            crate::process_engine::read_lines_lossy(reader, |line| tx.send(line).is_ok());
        });
    }
    if let Some(stdout) = child.stdout.take() {
        pump(stdout, tx.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        pump(stderr, tx.clone());
    }
    drop(tx);
    let mut collected = String::new();
    loop {
        if (ctx.cancelled)() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(cancelled());
        }
        let line = match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(line) => line,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                continue;
            }
        };
        // Checked as it arrives, so a prompt ends the run rather than
        // being discovered after it has sat waiting for an answer.
        if prompt_detected(&line) {
            let _ = child.kill();
            let _ = child.wait();
            collected.push_str(&line);
            return Ok(collected);
        }
        (ctx.on_progress)(Progress::Note {
            phase,
            message: line.clone(),
        });
        // Keep success markers and the recent tail, not an install's
        // potentially enormous lifetime output.
        if collected.len() > 65536
            && !collected.contains("Success! App")
            && !collected.contains("fully installed")
        {
            collected.clear();
        }
        collected.push_str(&line);
        collected.push('\n');
    }

    let _ = child.wait();
    Ok(collected)
}

/// Whether some output is asking a person to agree to something.
///
/// Deliberately broad. A false positive costs a person one puzzled look at a
/// message telling them to run steamcmd themselves; a false negative means a
/// program agreed to a licence on someone's behalf, which is not a mistake
/// this codebase gets to make twice.
pub fn prompt_detected(output: &str) -> bool {
    let lowered = output.to_ascii_lowercase();
    const SHAPES: [&str; 8] = [
        "license agreement",
        "licence agreement",
        "subscriber agreement",
        "terms of service",
        "do you accept",
        "accept the eula",
        "press 'i' to accept",
        "[y/n]",
    ];
    SHAPES.iter().any(|shape| lowered.contains(shape))
}

// ─── shared ─────────────────────────────────────────────────────────────────

fn stamp(dir: &Path, build_id: &str) -> Result<(), String> {
    fs::write(dir.join(STAMP), format!("{build_id}\n")).map_err(|_| cannot_write(dir))
}

fn cancelled() -> String {
    "the download was stopped.".to_string()
}

fn cannot_write(path: &Path) -> String {
    format!(
        "Homerun could not write to {}. Check there is space on the drive and that \
         the folder is not read-only.",
        path.display()
    )
}

fn cannot_read(path: &Path) -> String {
    format!("Homerun could not read {}.", path.display())
}

/// The host part of a URL, for a message a player reads.
///
/// The whole URL would be noise, and for a URL with a token in its query it
/// would be worse than noise.
fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("the download server")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── a vendor's downloader asking the person to sign in ────────────────

    fn hytale_markers() -> homerun_core::engine::descriptor::SignIn {
        homerun_core::engine::descriptor::SignIn {
            url: "oauth.accounts.hytale.com/oauth2/device/verify".into(),
            code: "Authorization code: ".into(),
        }
    }

    /// The shapes Hytale's downloader printed on 2026-09-23 (code redacted).
    #[test]
    fn the_verification_address_and_code_are_read_off_their_lines() {
        let m = hytale_markers();
        assert_eq!(
            sign_in_parts("Please visit the following URL to authenticate:", &m),
            (None, None)
        );
        assert_eq!(
            sign_in_parts(
                "https://oauth.accounts.hytale.com/oauth2/device/verify?user_code=AbCd1234",
                &m
            ),
            (
                Some(
                    "https://oauth.accounts.hytale.com/oauth2/device/verify?user_code=AbCd1234"
                        .into()
                ),
                None
            )
        );
        assert_eq!(
            sign_in_parts("Authorization code: AbCd1234", &m),
            (None, Some("AbCd1234".into()))
        );
    }

    #[test]
    fn only_an_https_address_is_ever_offered_to_open() {
        let m = hytale_markers();
        assert_eq!(
            sign_in_parts(
                "see http://oauth.accounts.hytale.com/oauth2/device/verify",
                &m
            )
            .0,
            None
        );
        assert_eq!(
            sign_in_parts("file://oauth.accounts.hytale.com/oauth2/device/verify", &m).0,
            None
        );
    }

    #[test]
    fn a_code_with_spaces_after_it_is_not_a_code() {
        let m = hytale_markers();
        assert_eq!(sign_in_parts("Authorization code: is required", &m).1, None);
        assert_eq!(sign_in_parts("Authorization code: ", &m).1, None);
    }

    #[test]
    fn no_markers_means_nothing_is_offered() {
        let m = homerun_core::engine::descriptor::SignIn::default();
        assert_eq!(
            sign_in_parts(
                "https://oauth.accounts.hytale.com/oauth2/device/verify?user_code=A",
                &m
            ),
            (None, None)
        );
    }

    /// Hytale's server, observed 2026-09-23: every line ends in a colour
    /// reset, even with `-Dterminal.ansi=false`. The address and code are
    /// passed on without it.
    #[test]
    fn terminal_escapes_are_not_part_of_an_address_or_a_code() {
        let m = homerun_core::engine::descriptor::SignIn {
            url: "oauth.accounts.hytale.com/oauth2/device/verify".into(),
            code: "Enter code: ".into(),
        };
        assert_eq!(
            sign_in_parts(
                "\u{1b}[m[INFO] [AbstractCommand] Or visit: https://oauth.accounts.hytale.com/oauth2/device/verify?user_code=AbCd\u{1b}[m",
                &m
            ).0.as_deref(),
            Some("https://oauth.accounts.hytale.com/oauth2/device/verify?user_code=AbCd")
        );
        assert_eq!(
            sign_in_parts(
                "\u{1b}[m[INFO] [AbstractCommand] Enter code: AbCd\u{1b}[m",
                &m
            )
            .1
            .as_deref(),
            Some("AbCd")
        );
        assert_eq!(without_escapes("\u{1b}[38;5;46mok\u{1b}[0m\u{1b}[m"), "ok");
    }

    /// The downloader prints the address with the code in it, then bare.
    /// Observed 2026-09-23: the bare one used to win.
    #[test]
    fn a_bare_address_never_replaces_the_one_with_the_code_in_it() {
        let full = "https://h.example/verify?user_code=AbCd";
        assert!(replaces_sign_in_url(None, "https://h.example/verify"));
        assert!(!replaces_sign_in_url(
            Some(full),
            "https://h.example/verify"
        ));
        assert!(replaces_sign_in_url(Some("https://h.example/verify"), full));
        assert!(!replaces_sign_in_url(Some(full), full));
    }
    use homerun_core::engine::descriptor::Extract;
    use std::sync::{Arc, Mutex};

    struct Recorder {
        progress: Arc<Mutex<Vec<Progress>>>,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                progress: Arc::new(Mutex::new(Vec::new())),
            }
        }
        fn phases(&self) -> Vec<&'static str> {
            self.progress
                .lock()
                .unwrap()
                .iter()
                .map(|p| match p {
                    Progress::Note { phase, .. } | Progress::Bytes { phase, .. } => *phase,
                    Progress::SignIn { .. } => "sign-in",
                })
                .collect()
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "homerun-fetch-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    /// A one-request HTTP server on loopback, so the download path is tested
    /// against a real socket rather than a mock of reqwest.
    fn serve(body: Vec<u8>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback");
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(4) {
                let Ok(mut stream) = stream else { continue };
                use std::io::BufRead;
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut range_from = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(rest) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        range_from = rest
                            .split('-')
                            .next()
                            .and_then(|n| n.trim().parse().ok())
                            .unwrap_or(0);
                    }
                }
                let slice = &body[range_from.min(body.len())..];
                let status = if range_from > 0 {
                    "206 Partial Content"
                } else {
                    "200 OK"
                };
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    slice.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(slice);
                let _ = stream.flush();
            }
        });
        format!("http://{address}/server.bin")
    }

    fn context<'a>(
        tools: &Path,
        recorder: &'a Recorder,
        cancelled: &'a dyn Fn() -> bool,
    ) -> Context<'a> {
        let progress = Arc::clone(&recorder.progress);
        // Leaked deliberately: a closure that borrows `progress` cannot also
        // be returned by value from here, and a test binary's lifetime is the
        // right lifetime for four bytes of Vec.
        let sink: &'a dyn Fn(Progress) = Box::leak(Box::new(move |p: Progress| {
            progress.lock().unwrap().push(p);
        }));
        Context {
            tools_dir: tools.to_path_buf(),
            on_progress: sink,
            cancelled,
        }
    }

    const NEVER: fn() -> bool = || false;

    #[test]
    fn a_pinned_download_is_verified_and_stamped() {
        let dir = scratch("direct");
        let body = b"a plausible little server binary".to_vec();
        let expected = {
            let mut h = Sha256::new();
            h.update(&body);
            h.finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        let url = serve(body);

        let recorder = Recorder::new();
        let ctx = context(&dir, &recorder, &NEVER);
        let plan = Plan::Direct {
            dir: dir.to_string_lossy().into_owned(),
            url,
            sha256: expected.clone(),
            size: None,
            extract: Extract::None,
            strip_components: 0,
        };

        let fetched = fetch(&plan, &ctx).expect("the download must succeed");
        assert_eq!(fetched.build_id, expected[..12]);
        assert_eq!(present(&dir).build_id.as_deref(), Some(&expected[..12]));
        assert!(
            dir.join("server.bin").exists(),
            "the vendor's file name is kept"
        );
        assert!(recorder.phases().contains(&"verify"));
    }

    /// The failure that matters most: bytes that are not the bytes we pinned
    /// must never be run, and must not be left behind to be resumed into.
    #[test]
    fn a_download_that_does_not_match_its_digest_is_refused_and_deleted() {
        let dir = scratch("corrupt");
        let url = serve(b"not what was promised".to_vec());
        let recorder = Recorder::new();
        let ctx = context(&dir, &recorder, &NEVER);

        let plan = Plan::Direct {
            dir: dir.to_string_lossy().into_owned(),
            url,
            sha256: "0".repeat(64),
            size: None,
            extract: Extract::None,
            strip_components: 0,
        };
        let err = fetch(&plan, &ctx).unwrap_err();
        assert!(err.contains("did not arrive intact"), "{err}");
        assert!(
            !dir.join(".download.part").exists(),
            "a file that failed its digest must not be left to resume into"
        );
        assert!(present(&dir).build_id.is_none(), "nothing was stamped");
    }

    #[test]
    fn an_interrupted_download_resumes_rather_than_starting_again() {
        let dir = scratch("resume");
        let body: Vec<u8> = (0..200_000u32).map(|n| (n % 251) as u8).collect();
        let expected = {
            let mut h = Sha256::new();
            h.update(&body);
            h.finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        let url = serve(body.clone());

        // Pretend a previous run got a third of the way.
        fs::write(dir.join(".download.part"), &body[..60_000]).unwrap();

        let recorder = Recorder::new();
        let ctx = context(&dir, &recorder, &NEVER);
        let plan = Plan::Direct {
            dir: dir.to_string_lossy().into_owned(),
            url,
            sha256: expected.clone(),
            size: Some(body.len() as u64),
            extract: Extract::None,
            strip_components: 0,
        };

        fetch(&plan, &ctx).expect("a resumed download must still verify");
        assert_eq!(
            fs::read(dir.join("server.bin")).unwrap(),
            body,
            "the resumed file must be the whole file"
        );
    }

    /// The reason the digest is checked after the download and not during it.
    #[test]
    fn a_part_that_is_already_complete_is_not_downloaded_again() {
        let dir = scratch("complete");
        let body = b"already here".to_vec();
        let expected = {
            let mut h = Sha256::new();
            h.update(&body);
            h.finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        fs::write(dir.join(".download.part"), &body).unwrap();

        let recorder = Recorder::new();
        let ctx = context(&dir, &recorder, &NEVER);
        // A URL nothing is serving: reaching the network at all would fail.
        let plan = Plan::Direct {
            dir: dir.to_string_lossy().into_owned(),
            url: "http://127.0.0.1:1/never".into(),
            sha256: expected,
            size: Some(body.len() as u64),
            extract: Extract::None,
            strip_components: 0,
        };
        fetch(&plan, &ctx).expect("a complete part needs no network");
    }

    #[test]
    fn an_already_present_runtime_is_returned_without_touching_anything() {
        let dir = scratch("present");
        let recorder = Recorder::new();
        let ctx = context(&dir, &recorder, &NEVER);
        let fetched = fetch(
            &Plan::AlreadyPresent {
                dir: dir.to_string_lossy().into_owned(),
                build_id: "abc123".into(),
            },
            &ctx,
        )
        .unwrap();
        assert_eq!(fetched.build_id, "abc123");
        assert!(recorder.phases().is_empty(), "nothing to report");
    }

    // ─── unpacking ──────────────────────────────────────────────────────────

    fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buffer = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buffer);
            let options: zip::write::FileOptions<()> = zip::write::FileOptions::default();
            for (name, body) in entries {
                writer.start_file(*name, options).unwrap();
                writer.write_all(body).unwrap();
            }
            writer.finish().unwrap();
        }
        buffer.into_inner()
    }

    #[test]
    fn an_archive_unpacks_into_the_runtime_directory() {
        let dir = scratch("zip");
        let archive = dir.join("a.zip");
        fs::write(
            &archive,
            zip_with(&[("server.exe", b"binary"), ("data/world.dat", b"world")]),
        )
        .unwrap();

        let recorder = Recorder::new();
        let ctx = context(&dir, &recorder, &NEVER);
        extract_zip(&archive, &dir, &ctx).expect("a well-formed archive unpacks");

        assert_eq!(fs::read(dir.join("server.exe")).unwrap(), b"binary");
        assert_eq!(fs::read(dir.join("data/world.dat")).unwrap(), b"world");
    }

    /// The digest pins the bytes, not the layout. A vendor archive nobody has
    /// audited entry-by-entry can still try to climb out.
    #[test]
    fn an_archive_that_climbs_out_of_its_directory_is_refused() {
        let dir = scratch("slip");
        let archive = dir.join("evil.zip");
        fs::write(
            &archive,
            zip_with(&[("../../escaped.txt", b"owned"), ("fine.txt", b"ok")]),
        )
        .unwrap();

        let recorder = Recorder::new();
        let ctx = context(&dir, &recorder, &NEVER);
        let err = extract_zip(&archive, &dir, &ctx).unwrap_err();
        assert!(err.contains("outside"), "{err}");

        let escaped = dir.parent().unwrap().parent().unwrap().join("escaped.txt");
        assert!(!escaped.exists(), "it wrote outside the directory anyway");
    }

    #[test]
    fn an_archive_with_an_absolute_path_is_refused() {
        let dir = scratch("absolute");
        let archive = dir.join("abs.zip");
        fs::write(&archive, zip_with(&[("/etc/passwd", b"nope")])).unwrap();
        let recorder = Recorder::new();
        let ctx = context(&dir, &recorder, &NEVER);
        assert!(extract_zip(&archive, &dir, &ctx).is_err());
    }

    #[test]
    fn something_that_is_not_an_archive_is_refused_in_words() {
        let dir = scratch("notzip");
        let archive = dir.join("not.zip");
        fs::write(&archive, b"this is not a zip file at all").unwrap();
        let recorder = Recorder::new();
        let ctx = context(&dir, &recorder, &NEVER);
        let err = extract_zip(&archive, &dir, &ctx).unwrap_err();
        assert!(err.contains("not an archive"), "{err}");
    }

    // ─── the rule about agreeing to things ──────────────────────────────────

    /// Deliberately broad, and deliberately tested as such: a false negative
    /// here means a program agreed to a licence on someone's behalf.
    // ─── steamcmd output that is not UTF-8 ─────────────────────────────────

    /// Not a test. Stands in for `steamcmd`, and is selected by name rather
    /// than by environment variable so that nothing has to be set in this
    /// process to spawn it. `cargo test` skips it because it is ignored.
    ///
    /// The bytes are the shape that broke this: steamcmd prints the paths it
    /// is installing into, and on a machine whose Windows username is not
    /// ASCII those arrive in the machine's code page rather than UTF-8. With
    /// the old reader that line ended the pump, `Success! App` was never
    /// seen, and the fetch failed — every time, on that machine, for ever.
    #[test]
    #[ignore = "spawned as a child by the test below"]
    fn i_am_fake_steamcmd() {
        use std::io::Write;
        let mut out = std::io::stdout();
        out.write_all(b"Redirecting stderr to 'C:\\steamcmd\\logs\\stderr.txt'\n")
            .unwrap();
        out.write_all(b"Logging directory: 'C:/Users/Fran\xe7ois/Steam/logs'\n")
            .unwrap();
        out.write_all(b" Update state (0x61) downloading, progress: 42.13\n")
            .unwrap();
        out.write_all(b"Success! App '258550' fully installed.\n")
            .unwrap();
        out.flush().unwrap();
        std::process::exit(0);
    }

    #[test]
    fn steamcmd_output_that_is_not_utf8_still_shows_the_line_that_matters() {
        let program = std::env::current_exe().expect("the test binary must be locatable");
        let args: Vec<String> = [
            "--exact",
            "fetcher::tests::i_am_fake_steamcmd",
            "--nocapture",
            "--ignored",
        ]
        .iter()
        .map(|a| (*a).to_string())
        .collect();

        let ctx = Context {
            tools_dir: std::env::temp_dir(),
            on_progress: &|_| {},
            cancelled: &|| false,
        };
        let output = run_streaming(&program, &args, "steamcmd", &ctx)
            .expect("a program that exits cleanly is not a failure");

        assert!(
            output.contains("Success! App"),
            "the pump stopped at the line that is not UTF-8: {output}"
        );
        assert!(
            output.contains('\u{fffd}'),
            "the bad byte vanished, so this is not the case it was meant to be: {output}"
        );
        assert!(
            !prompt_detected(&output),
            "an install was mistaken for a prompt: {output}"
        );
    }

    #[test]
    fn output_asking_someone_to_agree_is_recognised() {
        for asking in [
            "Please review the Steam Subscriber Agreement",
            "Do you accept the terms? [y/N]",
            "You must accept the EULA to continue",
            "END USER LICENSE AGREEMENT",
            "Press 'I' to accept",
        ] {
            assert!(prompt_detected(asking), "missed: {asking:?}");
        }
    }

    #[test]
    fn ordinary_output_is_not_mistaken_for_a_prompt() {
        for ordinary in [
            "Update state (0x61) downloading, progress: 42.11 (900 / 2137)",
            "Success! App '258550' fully installed.",
            "Logging in user 'anonymous' to Steam Public...",
            "",
        ] {
            assert!(!prompt_detected(ordinary), "false alarm: {ordinary:?}");
        }
    }

    // ─── update and verify are different questions ─────────────────────────

    fn args_of(build_id: Option<&str>, verify: bool) -> Vec<String> {
        steamcmd_args(Path::new("C:\rt\rust"), 258550, build_id, verify)
    }

    /// The measured cost: the Rust pilot re-verified 5,869,171,402 bytes on
    /// every start, minutes at a time, on a runtime that was already complete
    /// and already current. `validate` is what did that.
    #[test]
    fn an_ordinary_update_does_not_ask_for_every_file_to_be_read_again() {
        let args = args_of(None, false);
        assert!(
            !args.iter().any(|a| a == "validate"),
            "an ordinary update must not re-verify: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "+app_update"),
            "it must still ask Steam what changed: {args:?}"
        );
    }

    #[test]
    fn a_verify_asks_for_exactly_that_and_still_updates() {
        let args = args_of(None, true);
        assert!(args.iter().any(|a| a == "validate"), "{args:?}");
        assert!(args.iter().any(|a| a == "+app_update"), "{args:?}");
    }

    /// The order steamcmd actually requires, which is invisible when wrong:
    /// `+force_install_dir` after `+login` installs to steamcmd's own default
    /// directory instead, silently.
    #[test]
    fn the_install_directory_is_named_before_the_login() {
        let args = args_of(Some("1928"), true);
        let at = |needle: &str| args.iter().position(|a| a == needle).unwrap();
        assert!(at("+force_install_dir") < at("+login"), "{args:?}");
        assert!(at("+app_update") < at("validate"), "{args:?}");
        // A pin is a beta branch to steamcmd, and it belongs to the update.
        assert_eq!(args[at("-beta") + 1], "1928", "{args:?}");
        assert_eq!(args.last().unwrap(), "+quit", "{args:?}");
    }

    #[test]
    fn steamcmd_is_not_installed_from_anywhere_but_valve() {
        assert!(STEAMCMD_WINDOWS.starts_with("https://"));
        assert!(
            STEAMCMD_WINDOWS.contains("steamcdn-a.akamaihd.net")
                || STEAMCMD_WINDOWS.contains("steampowered.com"),
            "steamcmd must come from Valve: {STEAMCMD_WINDOWS}"
        );
    }

    #[test]
    fn a_named_steamcmd_that_is_not_there_is_refused_rather_than_bootstrapped_over() {
        let dir = scratch("override");
        let recorder = Recorder::new();
        let ctx = context(&dir, &recorder, &NEVER);
        // SAFETY: single-threaded test, and the variable is removed below.
        std::env::set_var(STEAMCMD_OVERRIDE, dir.join("no-such-steamcmd"));
        let result = steamcmd_binary(&ctx);
        std::env::remove_var(STEAMCMD_OVERRIDE);
        let err = result.unwrap_err();
        assert!(err.contains("not there"), "{err}");
    }

    // ─── cancellation and messages ──────────────────────────────────────────

    #[test]
    fn a_cancelled_download_stops_and_says_so() {
        let dir = scratch("cancel");
        let body: Vec<u8> = vec![7; 8 * 1024 * 1024];
        let url = serve(body);
        let recorder = Recorder::new();
        let always: fn() -> bool = || true;
        let ctx = context(&dir, &recorder, &always);

        let plan = Plan::Direct {
            dir: dir.to_string_lossy().into_owned(),
            url,
            sha256: "0".repeat(64),
            size: None,
            extract: Extract::None,
            strip_components: 0,
        };
        let err = fetch(&plan, &ctx).unwrap_err();
        assert!(err.contains("stopped"), "{err}");
    }

    #[test]
    fn a_download_server_that_is_not_there_is_named_without_the_whole_url() {
        let dir = scratch("unreachable");
        let recorder = Recorder::new();
        let ctx = context(&dir, &recorder, &NEVER);
        let plan = Plan::Direct {
            dir: dir.to_string_lossy().into_owned(),
            url: "http://127.0.0.1:1/secret-path?token=hunter2".into(),
            sha256: "0".repeat(64),
            size: None,
            extract: Extract::None,
            strip_components: 0,
        };
        let err = fetch(&plan, &ctx).unwrap_err();
        assert!(err.contains("127.0.0.1:1"), "{err}");
        assert!(!err.contains("hunter2"), "a token reached a message: {err}");
    }

    #[test]
    fn every_failure_reads_as_a_verdict() {
        let dir = scratch("messages");
        let recorder = Recorder::new();
        let ctx = context(&dir, &recorder, &NEVER);
        let err = fetch(
            &Plan::Direct {
                dir: dir.to_string_lossy().into_owned(),
                url: "http://127.0.0.1:1/x".into(),
                sha256: "0".repeat(64),
                size: None,
                extract: Extract::None,
                strip_components: 0,
            },
            &ctx,
        )
        .unwrap_err();
        for forbidden in ["unwrap", "panicked", "errno", "Err(", "os error"] {
            assert!(!err.contains(forbidden), "{err}");
        }
    }

    /// A directory with files but no stamp is not a runtime: it is what an
    /// interrupted download leaves behind, and treating it as finished is how
    /// a server starts against half an install.
    #[test]
    fn a_half_finished_directory_does_not_count_as_present() {
        let dir = scratch("halfway");
        fs::write(dir.join("server.exe"), b"partial").unwrap();
        assert!(present(&dir).build_id.is_none());
    }

    // ─── the vendor's own site, in a version the host chose ─────────────────

    /// A loopback server that answers each connection with the next canned
    /// response, whole. Returns its base address.
    fn serve_raw(responses: Vec<Vec<u8>>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback");
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                use std::io::BufRead;
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let _ = stream.write_all(&response);
                let _ = stream.flush();
            }
        });
        format!("http://{address}")
    }

    fn ok(body: &[u8]) -> Vec<u8> {
        let mut r = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        r.extend_from_slice(body);
        r
    }

    fn terraria_zip(marker: &[u8]) -> Vec<u8> {
        let mut buffer = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buffer);
            let options: zip::write::FileOptions<()> = zip::write::FileOptions::default();
            writer.add_directory("1458/", options).unwrap();
            writer
                .start_file("1458/Linux/TerrariaServer.exe", options)
                .unwrap();
            writer.write_all(marker).unwrap();
            writer
                .start_file("1458/Windows/serverconfig.txt", options)
                .unwrap();
            writer.write_all(b"config").unwrap();
            writer.finish().unwrap();
        }
        buffer.into_inner()
    }

    struct VendorCase {
        root: PathBuf,
        dir: PathBuf,
        record: PathBuf,
    }

    fn vendor_case(name: &str) -> VendorCase {
        let root = scratch(name);
        VendorCase {
            dir: root.join("terraria").join("1.4.5.8"),
            record: root.join("terraria").join(".vendor-hashes.json"),
            root,
        }
    }

    fn vendor_plan(case: &VendorCase, url: String, version: &str, size: Option<u64>) -> Plan {
        Plan::Vendor {
            dir: case
                .root
                .join("terraria")
                .join(version)
                .to_string_lossy()
                .into_owned(),
            url,
            version: version.into(),
            size,
            strip_components: 1,
            record: case.record.to_string_lossy().into_owned(),
        }
    }

    fn recorded(case: &VendorCase) -> std::collections::BTreeMap<String, String> {
        serde_json::from_slice(&fs::read(&case.record).unwrap()).unwrap()
    }

    #[test]
    fn a_first_vendor_download_is_unpacked_stripped_recorded_and_stamped() {
        let case = vendor_case("vendor-first");
        let body = terraria_zip(b"server v1");
        let base = serve_raw(vec![ok(&body)]);
        let recorder = Recorder::new();
        let ctx = context(&case.root, &recorder, &NEVER);

        let fetched = fetch(
            &vendor_plan(&case, format!("{base}/s-1458.zip"), "1.4.5.8", None),
            &ctx,
        )
        .expect("a first download is trusted");
        assert_eq!(fetched.build_id, "v1.4.5.8");
        assert_eq!(present(&case.dir).build_id.as_deref(), Some("v1.4.5.8"));
        assert_eq!(
            fs::read(case.dir.join("Linux/TerrariaServer.exe")).unwrap(),
            b"server v1",
            "the top folder is stripped"
        );
        assert!(case.dir.join("Windows/serverconfig.txt").exists());
        assert!(!case.dir.join("1458").exists());
        assert!(!case.dir.join(".download.part").exists());
        assert_eq!(recorded(&case)["1.4.5.8"], digest_of_bytes(&body));
        assert!(recorder.phases().contains(&"verify"));
        assert!(recorder.phases().contains(&"extract"));
    }

    fn digest_of_bytes(body: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(body);
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Trust on first use, per machine: the same version must be the same
    /// bytes every time after, and a vendor that swapped the file behind a
    /// version it already served is refused before anything is unpacked.
    #[test]
    fn a_changed_file_behind_a_version_already_seen_is_refused() {
        let case = vendor_case("vendor-changed");
        let first = terraria_zip(b"server v1");
        let same = first.clone();
        let changed = terraria_zip(b"server v1, quietly different");
        let base = serve_raw(vec![ok(&first), ok(&same), ok(&changed)]);
        let recorder = Recorder::new();
        let ctx = context(&case.root, &recorder, &NEVER);
        let plan = vendor_plan(&case, format!("{base}/s.zip"), "1.4.5.8", None);

        fetch(&plan, &ctx).expect("first");
        fs::remove_dir_all(&case.dir).unwrap();
        fetch(&plan, &ctx).expect("the same bytes again are the same version");

        fs::remove_dir_all(&case.dir).unwrap();
        let err = fetch(&plan, &ctx).unwrap_err();
        assert!(err.contains("changed"), "{err}");
        assert!(err.contains("1.4.5.8"), "{err}");
        assert!(present(&case.dir).build_id.is_none(), "nothing was stamped");
        assert!(
            !case.dir.join("Linux/TerrariaServer.exe").exists(),
            "nothing was unpacked"
        );
        assert!(!case.dir.join(".download.part").exists());
        assert_eq!(
            recorded(&case)["1.4.5.8"],
            digest_of_bytes(&first),
            "the record keeps what was seen first"
        );
    }

    #[test]
    fn each_version_is_recorded_on_its_own() {
        let case = vendor_case("vendor-two");
        let (a, b) = (terraria_zip(b"a"), terraria_zip(b"b"));
        let base = serve_raw(vec![ok(&a), ok(&b)]);
        let recorder = Recorder::new();
        let ctx = context(&case.root, &recorder, &NEVER);
        fetch(
            &vendor_plan(&case, format!("{base}/a.zip"), "1.4.5.8", None),
            &ctx,
        )
        .unwrap();
        fetch(
            &vendor_plan(&case, format!("{base}/b.zip"), "1.4.5.9", None),
            &ctx,
        )
        .unwrap();
        let record = recorded(&case);
        assert_eq!(record["1.4.5.8"], digest_of_bytes(&a));
        assert_eq!(record["1.4.5.9"], digest_of_bytes(&b));
        assert!(case
            .root
            .join("terraria/1.4.5.9/Linux/TerrariaServer.exe")
            .exists());
    }

    #[test]
    fn a_vendor_download_that_ends_early_is_refused_and_not_recorded() {
        let case = vendor_case("vendor-short");
        let body = terraria_zip(b"server");
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len() + 100
        )
        .into_bytes();
        response.extend_from_slice(&body);
        let base = serve_raw(vec![response]);
        let recorder = Recorder::new();
        let ctx = context(&case.root, &recorder, &NEVER);
        let err = fetch(
            &vendor_plan(&case, format!("{base}/s.zip"), "1.4.5.8", None),
            &ctx,
        )
        .unwrap_err();
        assert!(
            err.contains("ended before") || err.contains("interrupted"),
            "{err}"
        );
        assert!(
            !case.record.exists(),
            "a failed download is never the reference"
        );
        assert!(present(&case.dir).build_id.is_none());
    }

    #[test]
    fn a_vendor_download_of_the_wrong_size_is_refused() {
        let case = vendor_case("vendor-size");
        let body = terraria_zip(b"server");
        let base = serve_raw(vec![ok(&body)]);
        let recorder = Recorder::new();
        let ctx = context(&case.root, &recorder, &NEVER);
        let err = fetch(
            &vendor_plan(
                &case,
                format!("{base}/s.zip"),
                "1.4.5.8",
                Some(body.len() as u64 + 1),
            ),
            &ctx,
        )
        .unwrap_err();
        assert!(err.contains("not the size"), "{err}");
        assert!(!case.record.exists());
    }

    #[test]
    fn a_redirect_off_the_vendors_site_is_refused_and_one_on_it_is_followed() {
        // Another port is another origin.
        let elsewhere = serve_raw(vec![ok(&terraria_zip(b"not the vendor"))]);
        let case = vendor_case("vendor-redirect-off");
        let base = serve_raw(vec![format!(
            "HTTP/1.1 302 Found\r\nLocation: {elsewhere}/evil.zip\r\nContent-Length: 0\r\n\
             Connection: close\r\n\r\n"
        )
        .into_bytes()]);
        let recorder = Recorder::new();
        let ctx = context(&case.root, &recorder, &NEVER);
        let err = fetch(
            &vendor_plan(&case, format!("{base}/s.zip"), "1.4.5.8", None),
            &ctx,
        )
        .unwrap_err();
        assert!(err.contains("somewhere other than its own site"), "{err}");
        assert!(!case.record.exists());

        let case = vendor_case("vendor-redirect-on");
        // Same origin: the first response points at a path on the same site.
        let body = terraria_zip(b"moved");
        let base = serve_raw(vec![
            b"HTTP/1.1 302 Found\r\nLocation: /moved.zip\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec(),
            ok(&body),
        ]);
        fetch(
            &vendor_plan(&case, format!("{base}/s.zip"), "1.4.5.8", None),
            &ctx,
        )
        .expect("a redirect within the vendor's own site is followed");
        assert_eq!(
            fs::read(case.dir.join("Linux/TerrariaServer.exe")).unwrap(),
            b"moved"
        );
    }

    #[test]
    fn a_vendor_address_that_is_not_https_is_refused_without_a_request() {
        let case = vendor_case("vendor-http");
        let recorder = Recorder::new();
        let ctx = context(&case.root, &recorder, &NEVER);
        let err = fetch(
            &vendor_plan(
                &case,
                "http://terraria.org/s-1458.zip".into(),
                "1.4.5.8",
                None,
            ),
            &ctx,
        )
        .unwrap_err();
        assert!(err.contains("https"), "{err}");
        assert!(recorder.phases().is_empty(), "nothing was attempted");
    }

    #[test]
    fn a_record_that_cannot_be_read_is_refused_rather_than_forgotten() {
        let case = vendor_case("vendor-record");
        fs::create_dir_all(case.record.parent().unwrap()).unwrap();
        fs::write(&case.record, b"{ not json").unwrap();
        let recorder = Recorder::new();
        let ctx = context(&case.root, &recorder, &NEVER);
        let err = fetch(
            &vendor_plan(&case, "http://127.0.0.1:1/s.zip".into(), "1.4.5.8", None),
            &ctx,
        )
        .unwrap_err();
        assert!(err.contains("cannot be read"), "{err}");
        assert_eq!(fs::read(&case.record).unwrap(), b"{ not json");
    }

    // ─── stripping, and every member's CRC ──────────────────────────────────

    #[test]
    fn strip_components_drops_the_top_folder_and_keeps_the_rest() {
        let dir = scratch("strip");
        let archive = dir.join("a.zip");
        fs::write(&archive, terraria_zip(b"bin")).unwrap();
        let out = dir.join("out");
        let recorder = Recorder::new();
        let ctx = context(&dir, &recorder, &NEVER);
        extract_zip_stripped(&archive, &out, 1, &ctx).unwrap();
        assert_eq!(
            fs::read(out.join("Linux/TerrariaServer.exe")).unwrap(),
            b"bin"
        );
        assert!(!out.join("1458").exists());

        let two = dir.join("two");
        extract_zip_stripped(&archive, &two, 2, &ctx).unwrap();
        assert_eq!(fs::read(two.join("TerrariaServer.exe")).unwrap(), b"bin");
        assert_eq!(fs::read(two.join("serverconfig.txt")).unwrap(), b"config");
    }

    /// `a/../b` stays inside when nothing is dropped, and would be `../b` --
    /// beside the record of downloads -- once `a` is.
    #[test]
    fn stripping_cannot_turn_a_harmless_dot_dot_into_an_escape() {
        let dir = scratch("strip-escape");
        let archive = dir.join("evil.zip");
        fs::write(&archive, zip_with(&[("1458/../x.txt", b"up one")])).unwrap();
        let out = dir.join("out");
        let recorder = Recorder::new();
        let ctx = context(&dir, &recorder, &NEVER);
        let err = extract_zip_stripped(&archive, &out, 1, &ctx).unwrap_err();
        assert!(err.contains("outside"), "{err}");
        assert!(!dir.join("x.txt").exists());
    }

    fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buffer = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buffer);
            let options: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for (name, body) in entries {
                writer.start_file(*name, options).unwrap();
                writer.write_all(body).unwrap();
            }
            writer.finish().unwrap();
        }
        buffer.into_inner()
    }

    fn flip(bytes: &mut [u8], marker: &[u8]) {
        let at = bytes
            .windows(marker.len())
            .position(|w| w == marker)
            .expect("the marker is stored as written");
        bytes[at] ^= 0x01;
    }

    /// A member whose bytes do not match its CRC is refused -- including one
    /// that stripping leaves with no name, which is still read to its end.
    #[test]
    fn a_member_that_fails_its_crc_is_refused_even_when_it_is_stripped_away() {
        let dir = scratch("crc");
        let recorder = Recorder::new();
        let ctx = context(&dir, &recorder, &NEVER);

        let mut damaged = stored_zip(&[
            ("1458/ok.txt", b"fine"),
            ("1458/server.bin", b"MARKER-server-bytes"),
        ]);
        flip(&mut damaged, b"MARKER-server-bytes");
        let archive = dir.join("damaged.zip");
        fs::write(&archive, &damaged).unwrap();
        let err = extract_zip_stripped(&archive, &dir.join("out"), 1, &ctx).unwrap_err();
        assert!(err.contains("damaged"), "{err}");
        assert!(
            !dir.join("out/server.bin").exists(),
            "a member that failed its check is not left looking whole"
        );

        let mut top = stored_zip(&[("README-MARKER", b"TOPLEVEL-MARKER"), ("1458/a", b"a")]);
        flip(&mut top, b"TOPLEVEL-MARKER");
        let archive = dir.join("top.zip");
        fs::write(&archive, &top).unwrap();
        let err = extract_zip_stripped(&archive, &dir.join("top"), 1, &ctx).unwrap_err();
        assert!(err.contains("damaged"), "{err}");
    }
}
