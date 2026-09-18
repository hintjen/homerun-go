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
        } => direct(Path::new(dir), url, sha256, *size, *extract, ctx),
        Plan::SteamCmd {
            dir,
            app_id,
            build_id,
        } => steamcmd(Path::new(dir), *app_id, build_id.as_deref(), ctx),
    }
}

// ─── a pinned URL ───────────────────────────────────────────────────────────

fn direct(
    dir: &Path,
    url: &str,
    expected: &str,
    size: Option<u64>,
    extract: homerun_core::engine::descriptor::Extract,
    ctx: &Context,
) -> Result<Fetched, String> {
    use homerun_core::engine::descriptor::Extract;

    fs::create_dir_all(dir).map_err(|_| cannot_write(dir))?;

    // A partial file, so an interrupted download resumes instead of starting
    // a multi-gigabyte transfer again. It is never the finished artefact: the
    // rename happens only after the digest matches.
    let part = dir.join(".download.part");
    download(url, &part, size, ctx)?;

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
            extract_zip(&part, dir, ctx)?;
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

/// Stream a URL to a file, resuming a `.part` that is already there.
fn download(
    url: &str,
    part: &Path,
    expected_size: Option<u64>,
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
            return download(url, part, expected_size, ctx);
        }
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(None)
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|_| "Homerun could not start a download on this computer.".to_string())?;

    let mut request = client.get(url);
    if already > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={already}-"));
    }

    let mut response = request
        .send()
        .map_err(|_| format!("Homerun could not reach {}.", host_of(url)))?;

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
    let total = response
        .content_length()
        .map(|len| len + if resuming { already } else { 0 })
        .or(expected_size);

    let mut buffer = vec![0u8; 512 * 1024];
    let mut since_report = 0u64;
    loop {
        if (ctx.cancelled)() {
            return Err(cancelled());
        }
        let read = response
            .read(&mut buffer)
            .map_err(|_| format!("the download from {} was interrupted.", host_of(url)))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
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
    (ctx.on_progress)(Progress::Bytes {
        phase: "download",
        received,
        total,
    });
    Ok(())
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

        let target = into.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&target).map_err(|_| cannot_write(&target))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|_| cannot_write(parent))?;
        }

        let mut out = fs::File::create(&target).map_err(|_| cannot_write(&target))?;
        std::io::copy(&mut entry, &mut out).map_err(|_| cannot_write(&target))?;

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
    ctx: &Context,
) -> Result<Fetched, String> {
    let binary = steamcmd_binary(ctx)?;
    fs::create_dir_all(dir).map_err(|_| cannot_write(dir))?;

    (ctx.on_progress)(Progress::Note {
        phase: "steamcmd",
        message: "asking Steam for the server files".to_string(),
    });

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
    args.push("validate".to_string());
    args.push("+quit".to_string());

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
    download(STEAMCMD_WINDOWS, &archive, None, ctx)?;
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

    let stderr = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || {
            let mut text = String::new();
            let mut stderr = stderr;
            let _ = stderr.read_to_string(&mut text);
            text
        })
    });

    let mut collected = String::new();
    if let Some(stdout) = child.stdout.take() {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if (ctx.cancelled)() {
                let _ = child.kill();
                return Err(cancelled());
            }
            // Checked as it arrives, so a prompt ends the run rather than
            // being discovered after it has sat waiting for an answer.
            if prompt_detected(&line) {
                let _ = child.kill();
                collected.push_str(&line);
                return Ok(collected);
            }
            (ctx.on_progress)(Progress::Note {
                phase,
                message: line.clone(),
            });
            collected.push_str(&line);
            collected.push('\n');
        }
    }

    let _ = child.wait();
    if let Some(stderr) = stderr {
        if let Ok(text) = stderr.join() {
            collected.push_str(&text);
        }
    }
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
}
