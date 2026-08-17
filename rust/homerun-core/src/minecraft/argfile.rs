//! Turning Forge's and NeoForge's `@argfile`s into something a JNI-created VM
//! will accept.
//!
//! # Why this has to exist
//!
//! Forge and NeoForge do not hand you a runnable jar. Their installers
//! generate `run.sh`, `user_jvm_args.txt`, and a
//! `libraries/**/unix_args.txt`, and the launch is:
//!
//! ```text
//! java @user_jvm_args.txt @libraries/net/neoforged/neoforge/21.4.157/unix_args.txt nogui
//! ```
//!
//! **`@argfile` expansion is a feature of the `java` launcher binary**, not of
//! `JNI_CreateJavaVM`. Android has no `java` binary — the VM is created
//! in-process through JNI (`docs/android-server-backend.md`) — so nothing
//! expands them unless this does.
//!
//! # The part that is not obvious
//!
//! Expanding the file is not enough. The `java` launcher also *rewrites* the
//! options it forwards: it accepts `-p <path>` as two arguments and hands the
//! VM `--module-path=<path>`. The VM itself accepts only the joined form.
//!
//! Verified rather than reasoned about, against NeoForge 21.4.157 and a real
//! `JNI_CreateJavaVM`:
//!
//! ```text
//! -p libraries/…             -> Unrecognized option: -p, VM fails to start
//! --module-path=libraries/…  -> boots
//! ```
//!
//! Getting this wrong does not degrade anything. The VM refuses to start and
//! the only message is `Unrecognized option`.
//!
//! # Where the main class comes from
//!
//! The argfile carries the JVM options, the main class *and* the program
//! arguments, in that order — `cpw.mods.bootstraplauncher.BootstrapLauncher`
//! sits on its own line between them. The `java` launcher splits them by the
//! same rule used here: the first argument that is not an option, and not the
//! value of one, is the main class.

use serde::Serialize;

/// A launch, split the way `JNI_CreateJavaVM` and the host need it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Expanded {
    /// Ready to pass as `JavaVMOption`s — already in the joined form.
    #[serde(rename = "jvmOptions")]
    pub jvm_options: Vec<String>,
    /// The class to invoke, or `None` if the argfile named none.
    #[serde(rename = "mainClass")]
    pub main_class: Option<String>,
    /// Everything after the main class.
    #[serde(rename = "programArgs")]
    pub program_args: Vec<String>,
}

/// Options the `java` launcher takes as two arguments and the VM takes joined.
///
/// The value is the spelling the VM knows, which is why this is a map rather
/// than a set: `-p` is a launcher abbreviation the VM has never heard of.
const JOINED: [(&str, &str); 9] = [
    ("-p", "--module-path"),
    ("--module-path", "--module-path"),
    ("--add-modules", "--add-modules"),
    ("--add-opens", "--add-opens"),
    ("--add-exports", "--add-exports"),
    ("--add-reads", "--add-reads"),
    ("--patch-module", "--patch-module"),
    ("--limit-modules", "--limit-modules"),
    ("--upgrade-module-path", "--upgrade-module-path"),
];

/// The class path, which the VM knows only as a system property.
const CLASS_PATH: [&str; 3] = ["-cp", "-classpath", "--class-path"];

/// Read one or more argfiles, in order, into a launch.
///
/// [`contents`] is each file's text, concatenated in the order the run script
/// names them — `user_jvm_args.txt` first, so a heap setting there is seen
/// before the loader's own options.
pub fn expand(contents: &[String]) -> Expanded {
    let tokens: Vec<String> = contents.iter().flat_map(|text| tokenize(text)).collect();

    let mut out = Expanded::default();
    let mut i = 0;
    while i < tokens.len() {
        let token = &tokens[i];

        // Everything after the main class belongs to the program, including
        // arguments that look like options — `--launchTarget neoforgeserver`
        // is ModLauncher's, not the VM's.
        if out.main_class.is_some() {
            out.program_args.push(token.clone());
            i += 1;
            continue;
        }

        if let Some((_, vm_spelling)) = JOINED.iter().find(|(launcher, _)| launcher == token) {
            // A missing value is a truncated argfile. Dropping the option is
            // the honest response: passing a bare `-p` fails the VM outright,
            // and inventing a value would be worse.
            if let Some(value) = tokens.get(i + 1) {
                out.jvm_options.push(format!("{vm_spelling}={value}"));
            }
            i += 2;
            continue;
        }

        if CLASS_PATH.contains(&token.as_str()) {
            if let Some(value) = tokens.get(i + 1) {
                out.jvm_options.push(format!("-Djava.class.path={value}"));
            }
            i += 2;
            continue;
        }

        if token.starts_with('-') {
            out.jvm_options.push(token.clone());
            i += 1;
            continue;
        }

        out.main_class = Some(token.clone());
        i += 1;
    }
    out
}

/// Split argfile text into arguments.
///
/// The JDK's `@argfile` grammar, which is **not** shell:
///
///  - whitespace separates arguments;
///  - single or double quotes group one, and may be partial (`-Dx="a b"`);
///  - inside quotes, `\` escapes the usual set;
///  - `#` outside quotes starts a comment to end of line.
///
/// Comments matter more than they look: a freshly generated
/// `user_jvm_args.txt` is **nothing but** comments — NeoForge ships it with
/// every line commented out and an invitation to uncomment `-Xmx4G`. Treating
/// those as arguments hands the VM a line of prose.
///
/// Line continuation (a trailing `\`) is not supported. It cannot arise here:
/// the loader-generated argfiles do not use it, and `user_jvm_args.txt` is
/// rewritten by the host on every start, so a hand-edited one does not
/// survive to be parsed.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();

    for line in text.lines() {
        let mut current = String::new();
        let mut started = false;
        let mut quote: Option<char> = None;
        let mut chars = line.chars().peekable();

        while let Some(c) = chars.next() {
            match c {
                '#' if quote.is_none() => break, // comment to end of line
                '\'' | '"' if quote.is_none() => {
                    quote = Some(c);
                    started = true;
                }
                c if Some(c) == quote => quote = None,
                '\\' if quote.is_some() => {
                    // Only inside quotes: an unquoted backslash is a path
                    // separator on some hosts and must survive untouched.
                    match chars.next() {
                        Some('n') => current.push('\n'),
                        Some('r') => current.push('\r'),
                        Some('t') => current.push('\t'),
                        Some('f') => current.push('\u{000C}'),
                        Some(other) => current.push(other),
                        None => current.push('\\'),
                    }
                    started = true;
                }
                c if c.is_whitespace() && quote.is_none() => {
                    if started {
                        out.push(std::mem::take(&mut current));
                        started = false;
                    }
                }
                c => {
                    current.push(c);
                    started = true;
                }
            }
        }
        if started {
            out.push(current);
        }
    }
    out
}

/// The argfiles a Forge or NeoForge run script names, in the order it names
/// them.
///
/// Reads the generated script rather than guessing paths, which is what
/// docker-minecraft-server does and what the desktop copied
/// (`findLoaderArgfiles`, `mod-installer.ts:408`). The version is in the path
/// — `libraries/net/neoforged/neoforge/21.4.157/unix_args.txt` — so guessing
/// means knowing the build, and the script already knows it.
///
/// Returned paths are relative to the server directory, as written.
pub fn referenced_argfiles(run_script: &str) -> Vec<String> {
    let mut found = Vec::new();
    let bytes = run_script.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'@' {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut end = start;
        // The desktop's `[^\s%"]+` — `%` excluded because run.bat writes
        // `%*` and a Windows variable would otherwise be swallowed into the
        // path.
        while end < bytes.len()
            && !bytes[end].is_ascii_whitespace()
            && bytes[end] != b'%'
            && bytes[end] != b'"'
        {
            end += 1;
        }
        let candidate = &run_script[start..end];
        if candidate.starts_with("libraries") && candidate.to_ascii_lowercase().ends_with(".txt") {
            found.push(candidate.to_string());
        }
        i = end.max(start + 1);
    }
    found
}

/// Which run script to believe, given which exist.
///
/// **`run.sh` before `run.bat`**, always. Both are generated, and the Windows
/// one names `win_args.txt`, whose class path uses `;` separators and `\`
/// paths. Feeding that to a VM on Android produces
/// `InvalidPathException: Illegal char <:>` or a missing
/// `cpw.mods.bootstraplauncher.BootstrapLauncher`, depending on which way
/// round you get it — the desktop hit exactly this and reordered for it.
pub fn preferred_run_script(present: &[String]) -> Option<&str> {
    ["run.sh", "run.bat"]
        .into_iter()
        .find(|name| present.iter().any(|p| p == name))
}

/// The argfile to fall back to when no run script names one, given every path
/// under `libraries/`.
///
/// Same preference and the same reason: the host's own argfile first, the
/// other only if there is nothing else.
pub fn fallback_argfile(library_paths: &[String]) -> Option<&str> {
    let unix = library_paths
        .iter()
        .find(|p| p.ends_with("unix_args.txt"))
        .map(String::as_str);
    unix.or_else(|| {
        library_paths
            .iter()
            .find(|p| p.ends_with("win_args.txt") || p.ends_with("win_server_args.txt"))
            .map(String::as_str)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real thing, from `neoforge-21.4.157-installer.jar --installServer`.
    /// Trimmed to the lines that matter; the full file is in
    /// `shared/fixtures/argfiles/`.
    const NEOFORGE: &str = "\
-p libraries/cpw/mods/bootstraplauncher/2.0.2/bootstraplauncher-2.0.2.jar:libraries/cpw/mods/securejarhandler/3.0.8/securejarhandler-3.0.8.jar
--add-modules ALL-MODULE-PATH
--add-opens java.base/java.util.jar=cpw.mods.securejarhandler
--add-exports java.base/sun.security.util=cpw.mods.securejarhandler
-Djava.net.preferIPv6Addresses=system
-DlibraryDirectory=libraries
cpw.mods.bootstraplauncher.BootstrapLauncher
--launchTarget neoforgeserver
--fml.neoForgeVersion 21.4.157
";

    /// The heap file NeoForge actually ships: every line a comment.
    const USER_JVM_ARGS: &str = "\
# Xmx and Xms set the maximum and minimum RAM usage, respectively.
# For example, to set the maximum to 3GB: -Xmx3G

# A good default for a modded server is 4GB.
# Uncomment the next line to set it.
# -Xmx4G
";

    #[test]
    fn a_real_neoforge_argfile_splits_into_a_launch() {
        let out = expand(&[NEOFORGE.to_string()]);

        assert_eq!(
            out.main_class.as_deref(),
            Some("cpw.mods.bootstraplauncher.BootstrapLauncher")
        );
        assert_eq!(
            out.program_args,
            vec![
                "--launchTarget",
                "neoforgeserver",
                "--fml.neoForgeVersion",
                "21.4.157"
            ]
        );
    }

    /// The fix the whole module exists for. `-p libraries/…` as two arguments
    /// fails `JNI_CreateJavaVM` with `Unrecognized option: -p`; the joined
    /// form boots. Checked against a real VM, not inferred.
    #[test]
    fn two_token_options_are_joined_and_short_forms_are_spelled_out() {
        let out = expand(&[NEOFORGE.to_string()]);

        assert!(
            out.jvm_options
                .iter()
                .any(|o| o.starts_with("--module-path=libraries/cpw/mods/bootstraplauncher")),
            "-p must become --module-path=: {:?}",
            out.jvm_options
        );
        assert!(out
            .jvm_options
            .contains(&"--add-modules=ALL-MODULE-PATH".to_string()));
        assert!(out.jvm_options.contains(
            &"--add-opens=java.base/java.util.jar=cpw.mods.securejarhandler".to_string()
        ));
        assert!(
            !out.jvm_options.iter().any(|o| o == "-p"),
            "{:?}",
            out.jvm_options
        );
    }

    #[test]
    fn options_that_carry_their_own_value_are_passed_through() {
        let out = expand(&[NEOFORGE.to_string()]);
        assert!(out
            .jvm_options
            .contains(&"-Djava.net.preferIPv6Addresses=system".to_string()));
        assert!(out
            .jvm_options
            .contains(&"-DlibraryDirectory=libraries".to_string()));
    }

    #[test]
    fn a_class_path_becomes_the_property_the_vm_knows() {
        let out = expand(&["-cp a.jar:b.jar Main".to_string()]);
        assert_eq!(out.jvm_options, vec!["-Djava.class.path=a.jar:b.jar"]);
        assert_eq!(out.main_class.as_deref(), Some("Main"));
    }

    /// A freshly generated `user_jvm_args.txt` is nothing but comments.
    /// Treating them as arguments hands the VM a line of prose.
    #[test]
    fn a_comment_only_argfile_contributes_nothing() {
        assert_eq!(expand(&[USER_JVM_ARGS.to_string()]), Expanded::default());
    }

    #[test]
    fn files_are_read_in_the_order_the_run_script_names_them() {
        let out = expand(&["-Xmx2048M\n-Xms2048M\n".to_string(), NEOFORGE.to_string()]);
        assert_eq!(out.jvm_options[0], "-Xmx2048M");
        assert_eq!(out.jvm_options[1], "-Xms2048M");
        assert_eq!(
            out.main_class.as_deref(),
            Some("cpw.mods.bootstraplauncher.BootstrapLauncher")
        );
    }

    /// Everything after the main class is the program's, including things
    /// that look like JVM options.
    #[test]
    fn program_arguments_are_not_reinterpreted_as_options() {
        let out = expand(&["-Xmx1G Main --add-opens something -Dnot.a.vm.option=1".to_string()]);
        assert_eq!(out.jvm_options, vec!["-Xmx1G"]);
        assert_eq!(
            out.program_args,
            vec!["--add-opens", "something", "-Dnot.a.vm.option=1"]
        );
    }

    #[test]
    fn quotes_group_an_argument_and_may_be_partial() {
        assert_eq!(
            tokenize(r#"-Dname="a b" 'c d' -Dx=y"#),
            vec!["-Dname=a b", "c d", "-Dx=y"]
        );
    }

    /// An unquoted backslash is a path separator on some hosts and must
    /// survive; inside quotes it escapes.
    #[test]
    fn backslashes_escape_only_inside_quotes() {
        assert_eq!(tokenize(r"-Dpath=C:\temp\x"), vec![r"-Dpath=C:\temp\x"]);
        assert_eq!(tokenize(r#""a\"b""#), vec![r#"a"b"#]);
        assert_eq!(tokenize(r#""a\tb""#), vec!["a\tb"]);
    }

    #[test]
    fn a_hash_inside_quotes_is_not_a_comment() {
        assert_eq!(tokenize(r#"-Dmotd="a # b" # gone"#), vec!["-Dmotd=a # b"]);
    }

    #[test]
    fn a_truncated_option_is_dropped_rather_than_passed_bare() {
        // A bare `-p` fails the VM outright, so emitting nothing is the only
        // response that leaves the server able to start.
        let out = expand(&["-Xmx1G -p".to_string()]);
        assert_eq!(out.jvm_options, vec!["-Xmx1G"]);
    }

    // --- the real files -------------------------------------------------

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/../../shared/fixtures/argfiles/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    /// The whole chain, on the files a real installer produced.
    ///
    /// These fixtures came out of `neoforge-21.4.157-installer.jar
    /// --installServer`, and the expansion asserted here is the one that was
    /// fed to a real `JNI_CreateJavaVM` and booted: ModLauncher started, mods
    /// were discovered, and NeoForge 21.4.157 loaded for Minecraft 1.21.4.
    ///
    /// So this is a regression test against a launch that is known to work,
    /// not against my reading of a format.
    #[test]
    fn the_files_a_real_neoforge_install_produced_expand_into_the_launch_that_booted() {
        let script = fixture("neoforge-21.4.157-run.sh");
        let named = referenced_argfiles(&script);
        assert_eq!(
            named,
            vec!["libraries/net/neoforged/neoforge/21.4.157/unix_args.txt"],
            "the run script names exactly the loader's argfile, not user_jvm_args.txt"
        );

        let out = expand(&[
            fixture("neoforge-21.4.157-user_jvm_args.txt"),
            fixture("neoforge-21.4.157-unix_args.txt"),
        ]);

        assert_eq!(
            out.main_class.as_deref(),
            Some("cpw.mods.bootstraplauncher.BootstrapLauncher")
        );
        assert_eq!(
            out.program_args,
            vec![
                "--launchTarget",
                "neoforgeserver",
                "--fml.neoForgeVersion",
                "21.4.157",
                "--fml.fmlVersion",
                "6.0.18",
                "--fml.mcVersion",
                "1.21.4",
                "--fml.neoFormVersion",
                "20241203.161809",
            ]
        );

        // Nothing may reach the VM in the launcher's two-token spelling.
        for option in &out.jvm_options {
            assert!(
                option.starts_with('-'),
                "a bare value leaked into the options: {option}"
            );
            assert!(
                !JOINED.iter().any(|(launcher, _)| launcher == option),
                "{option} needed joining and did not get it"
            );
        }

        // And the module path is the joined form, with `:` — the separator
        // Android uses, and the reason `unix_args.txt` is the one to read.
        let module_path = out
            .jvm_options
            .iter()
            .find(|o| o.starts_with("--module-path="))
            .expect("a module path");
        assert!(module_path.contains(".jar:libraries/"), "{module_path}");
    }

    /// Byte-for-byte against the argv that booted.
    ///
    /// `neoforge-21.4.157-expected-argv.txt` is the exact argument vector fed
    /// to `homerun-java-launcher` and `JNI_CreateJavaVM` in the run that
    /// worked — one line per argument, main class first, then the JVM options,
    /// then `--`, then the program arguments, which is the launcher's own
    /// contract.
    ///
    /// The tests above assert properties; this asserts the answer. A change
    /// that keeps every property and still alters one option fails here, which
    /// is the point — the properties were written by the same person who could
    /// be wrong about them, and this file was written by a JVM that started.
    #[test]
    fn the_expansion_is_exactly_the_argv_that_booted() {
        let expected = fixture("neoforge-21.4.157-expected-argv.txt");
        let mut lines = expected.lines().map(str::trim_end);

        let main_class = lines.next().expect("a main class").to_string();
        let jvm: Vec<String> = lines
            .by_ref()
            .take_while(|l| *l != "--")
            .map(String::from)
            .collect();
        let program: Vec<String> = lines.map(String::from).collect();

        let out = expand(&[
            fixture("neoforge-21.4.157-user_jvm_args.txt"),
            fixture("neoforge-21.4.157-unix_args.txt"),
        ]);

        assert_eq!(out.main_class, Some(main_class));
        assert_eq!(out.jvm_options, jvm);
        assert_eq!(out.program_args, program);
    }

    // --- finding the files ---------------------------------------------

    #[test]
    fn the_run_script_names_its_argfile() {
        let script = "#!/usr/bin/env sh\n\
                      java @user_jvm_args.txt \
                      @libraries/net/neoforged/neoforge/21.4.157/unix_args.txt \"$@\"\n";
        assert_eq!(
            referenced_argfiles(script),
            vec!["libraries/net/neoforged/neoforge/21.4.157/unix_args.txt"]
        );
    }

    /// `run.bat` writes `%*`, and swallowing a Windows variable into the path
    /// is what the `%` exclusion is for.
    #[test]
    fn a_windows_run_script_does_not_swallow_its_variables() {
        let script = "java @user_jvm_args.txt \
                      @libraries/net/neoforged/neoforge/21.4.157/win_args.txt %*\n";
        assert_eq!(
            referenced_argfiles(script),
            vec!["libraries/net/neoforged/neoforge/21.4.157/win_args.txt"]
        );
    }

    /// Both scripts are always generated. Believing the Windows one on Android
    /// gives a class path with `;` separators and `\` paths.
    #[test]
    fn the_unix_run_script_wins_whenever_it_exists() {
        let both = vec!["run.bat".to_string(), "run.sh".to_string()];
        assert_eq!(preferred_run_script(&both), Some("run.sh"));
        assert_eq!(
            preferred_run_script(&["run.bat".to_string()]),
            Some("run.bat")
        );
        assert_eq!(preferred_run_script(&[]), None);
    }

    #[test]
    fn the_fallback_prefers_the_unix_argfile_too() {
        let paths = vec![
            "libraries/x/win_args.txt".to_string(),
            "libraries/x/unix_args.txt".to_string(),
        ];
        assert_eq!(fallback_argfile(&paths), Some("libraries/x/unix_args.txt"));
        assert_eq!(
            fallback_argfile(&["libraries/x/win_args.txt".to_string()]),
            Some("libraries/x/win_args.txt"),
            "better than refusing to launch at all"
        );
        assert_eq!(fallback_argfile(&[]), None);
    }
}
