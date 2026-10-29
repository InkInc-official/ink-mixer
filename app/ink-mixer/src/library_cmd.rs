//! `ink-mixer library`: look at and change the sound library from the command
//! line (ADR-0010).
//!
//! - `path`: print the folder of the library. Nothing is made or opened.
//! - `list`: one line for each sound, with tabs between the columns: name, ID,
//!   kind, state, length, path. Nothing is written.
//! - `scan`: look at the sound folders for files that were put in by hand.
//! - `import <FILE> --kind bgm|se`: copy a file into the library.
//!
//! What a command found or failed at goes to stderr, one line each; the result
//! that a script may want (the folder, the list, the ID of what was imported)
//! goes to stdout (AGENTS.md section 10). The exit code is 0 for success and 1
//! for failure; an error in the arguments is 2 (clap).
//!
//! A library index that cannot be read is never overwritten, and is not moved
//! by this command either: the message tells the user how to move it aside by
//! hand.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use directories::ProjectDirs;
use ink_core::{
    Asset, AssetId, AssetInfo, AssetKind, AssetState, ImportError, ImportOutcome, IndexWarning,
    Library, LibraryError, ProbeFailure, ProbeFn, ScanReport, SkipReason, Skipped,
};
use ink_media::asset_info;

use crate::cli::{ImportArgs, LibraryAction, LibraryArgs};

/// The name of the index file, for the messages (the library owns the file).
const INDEX_FILE: &str = "library.toml";

/// The folder of the library on this PC: the local data folder of the OS
/// (`~/.local/share/inkmixer` on Linux, `%LOCALAPPDATA%\Ink Inc\Ink Mixer\data`
/// on Windows). `None` if the OS does not tell where it is.
pub fn library_root() -> Option<PathBuf> {
    ProjectDirs::from("", "Ink Inc", "Ink Mixer").map(|dirs| dirs.data_local_dir().to_path_buf())
}

/// Chooses the folder of the library: the one given with `--library-dir` if
/// there is one, or else the default one. A relative path is kept as it is
/// (the library makes it absolute).
fn resolve_root(
    given: Option<&Path>,
    default_root: Option<PathBuf>,
) -> Result<PathBuf, &'static str> {
    match (given, default_root) {
        (Some(given), _) => Ok(given.to_path_buf()),
        (None, Some(default_root)) => Ok(default_root),
        (None, None) => {
            Err("cannot find the data folder of this PC; give a folder with --library-dir")
        }
    }
}

/// Runs `ink-mixer library ...`.
pub fn run(args: &LibraryArgs) -> ExitCode {
    let ok = execute(
        args,
        library_root(),
        &asset_info,
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    );
    exit_code(ok)
}

fn exit_code(ok: bool) -> ExitCode {
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Does what `args` ask. Returns whether it succeeded. The default folder and
/// the function that reads sound files are given, so that a test can use
/// others.
fn execute(
    args: &LibraryArgs,
    default_root: Option<PathBuf>,
    probe: ProbeFn<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> bool {
    let root = match resolve_root(args.library_dir.as_deref(), default_root) {
        Ok(root) => root,
        Err(message) => {
            say(err, &format!("error: {message}"));
            return false;
        }
    };
    match &args.action {
        LibraryAction::Path => print_path(&root, out, err),
        LibraryAction::List => list(&root, out, err),
        LibraryAction::Scan => scan(&root, probe, out, err),
        LibraryAction::Import(import) => import_file(&root, import, probe, out, err),
    }
}

fn say(w: &mut dyn Write, line: &str) {
    // A closed pipe (`| head`) is not an error of this command.
    let _ = writeln!(w, "{line}");
}

fn print_path(root: &Path, out: &mut dyn Write, err: &mut dyn Write) -> bool {
    match std::path::absolute(root) {
        Ok(path) => {
            say(out, &path.display().to_string());
            true
        }
        Err(e) => {
            say(
                err,
                &message_line(&format!("error: {}: {e}", root.display())),
            );
            false
        }
    }
}

fn list(root: &Path, out: &mut dyn Write, err: &mut dyn Write) -> bool {
    let Some(library) = open(root, err) else {
        return false;
    };
    if library.assets().is_empty() {
        say(
            err,
            &format!(
                "note: the library has no sounds; put WAV or MP3 files in {} or {} and run `library scan`, or use `library import`",
                library.folder(AssetKind::Bgm).display(),
                library.folder(AssetKind::Se).display()
            ),
        );
    }
    for asset in library.assets() {
        let path = library.path_of(asset.id());
        say(out, &format_list_line(asset, path.as_deref()));
    }
    true
}

fn scan(root: &Path, probe: ProbeFn<'_>, out: &mut dyn Write, err: &mut dyn Write) -> bool {
    let Some(mut library) = open(root, err) else {
        return false;
    };
    match library.scan(probe) {
        Ok(report) => {
            let report = complete_report(report, &library, probe);
            let describe = |id: &AssetId| describe_asset(&library, id);
            for line in format_scan_report(&report, &describe) {
                say(err, &line);
            }
            say(out, &format_scan_summary(&report));
            true
        }
        Err(e) => {
            say(err, &message_line(&format!("error: {e}")));
            false
        }
    }
}

fn import_file(
    root: &Path,
    args: &ImportArgs,
    probe: ProbeFn<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> bool {
    let Some(mut library) = open(root, err) else {
        return false;
    };
    match library.import(&args.file, args.kind.into(), probe) {
        Ok(outcome) => {
            let (word, id) = match outcome {
                ImportOutcome::Imported(id) => ("imported", id),
                ImportOutcome::AlreadyThere(id) => ("already there", id),
            };
            let path = library.path_of(&id);
            say(out, &format_import_result(word, &id, path.as_deref()));
            true
        }
        Err(e) => {
            say(err, &import_error_line(&e));
            false
        }
    }
}

/// Opens the library. What is wrong with the index is told on stderr; a
/// library that cannot be opened is `None` and nothing was written.
fn open(root: &Path, err: &mut dyn Write) -> Option<Library> {
    match Library::open(root) {
        Ok(opened) => {
            for line in format_warnings(&opened.warnings) {
                say(err, &line);
            }
            Some(opened.library)
        }
        Err(e) => {
            for line in open_error_lines(root, &e) {
                say(err, &line);
            }
            None
        }
    }
}

/// A scan tells what changed while it ran, but each run of this command starts
/// from the files (the state of a file is not saved). So that a file that went
/// away in the meantime, or one that cannot be read, is not left unsaid, every
/// sound that is missing or unreadable now is added to the report. For one that
/// was unreadable already, the file is read once more to tell why.
fn complete_report(mut report: ScanReport, library: &Library, probe: ProbeFn<'_>) -> ScanReport {
    for asset in library.assets() {
        let id = *asset.id();
        match asset.state() {
            AssetState::Missing if !report.missing.contains(&id) => report.missing.push(id),
            AssetState::Unreadable if !report.unreadable.iter().any(|(told, _)| *told == id) => {
                let failure = library
                    .path_of(&id)
                    .and_then(|path| probe(&path).err())
                    .unwrap_or_else(|| {
                        ProbeFailure::Unreadable("the file cannot be read".to_string())
                    });
                report.unreadable.push((id, failure));
            }
            _ => {}
        }
    }
    report
}

fn describe_asset(library: &Library, id: &AssetId) -> String {
    match library.get(id) {
        Some(asset) => format!("{id} {}", asset.relative_path()),
        None => id.to_string(),
    }
}

// -- The text of the output: pure functions -------------------------------------------

/// Makes a message one line without runs of spaces: what a library said over
/// several lines (a TOML error shows the line and a mark under it) is joined
/// with single spaces.
fn message_line(text: &str) -> String {
    one_line(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Makes `text` one line: a line break, a tab or another control character
/// becomes a space.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn state_word(state: AssetState) -> &'static str {
    match state {
        AssetState::Available => "available",
        AssetState::Missing => "missing",
        AssetState::Unreadable => "unreadable",
    }
}

/// The length for the list: `-` if the file was never read, `?` if it was read
/// but does not say how long it is, else `m:ss.mmm` (`h:mm:ss.mmm` from an
/// hour). The milliseconds are cut, not rounded.
fn format_length(info: Option<&AssetInfo>) -> String {
    let Some(info) = info else {
        return "-".to_string();
    };
    let (Some(frames), rate) = (info.frames, info.sample_rate) else {
        return "?".to_string();
    };
    if rate == 0 {
        return "?".to_string();
    }
    let millis = u128::from(frames) * 1000 / u128::from(rate);
    format_millis(millis)
}

fn format_millis(millis: u128) -> String {
    let hours = millis / 3_600_000;
    let minutes = millis / 60_000 % 60;
    let seconds = millis / 1000 % 60;
    let millis = millis % 1000;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}.{millis:03}")
    } else {
        format!("{}:{seconds:02}.{millis:03}", minutes)
    }
}

/// One line of the list: name, ID, kind, state, length, path, with tabs
/// between. The path is where the file is (or would be); it is empty only for
/// an Asset the library does not know.
fn format_list_line(asset: &Asset, path: Option<&Path>) -> String {
    let path = path.map(|p| p.display().to_string()).unwrap_or_default();
    [
        one_line(asset.name()),
        asset.id().to_string(),
        asset.kind().folder_name().to_string(),
        state_word(asset.state()).to_string(),
        format_length(asset.info()),
        one_line(&path),
    ]
    .join("\t")
}

/// The warnings of the index, one line each, and one line to say what they
/// mean for the next save.
fn format_warnings(warnings: &[IndexWarning]) -> Vec<String> {
    if warnings.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<String> = warnings
        .iter()
        .map(|warning| message_line(&format!("warning: {INDEX_FILE}: {warning}")))
        .collect();
    lines.push(format!(
        "warning: {} problem(s) in {INDEX_FILE}; an entry that was left out is lost from the index at the next save (scan or import)",
        warnings.len()
    ));
    lines
}

/// What to tell when the library cannot be opened. An index that cannot be used
/// gets a second line: how to move it aside by hand (nothing is moved here).
fn open_error_lines(root: &Path, error: &LibraryError) -> Vec<String> {
    let first = match error {
        LibraryError::Index(e) => {
            let index = root.join(INDEX_FILE);
            format!("error: {} cannot be used: {e}", index.display())
        }
        other => format!("error: {other}"),
    };
    let mut lines = vec![message_line(&first)];
    if matches!(error, LibraryError::Index(_)) {
        lines.push(format!(
            "hint: move {INDEX_FILE} to another name such as {INDEX_FILE}.broken (do not delete it) and run the command again; new IDs are given, and the display names and trigger words are lost"
        ));
    }
    lines
}

fn skip_reason_text(reason: &SkipReason) -> String {
    match reason {
        SkipReason::UnsupportedExtension => "the extension is not wav or mp3".to_string(),
        SkipReason::NameNotUtf8 => "the name is not UTF-8".to_string(),
        SkipReason::UnsafeName(problem) => format!("the name cannot be used: {problem}"),
        SkipReason::TooDeep => "the folder is too deep".to_string(),
        SkipReason::Symlink => "a link is not followed".to_string(),
        SkipReason::NotReadable(kind) => format!("cannot be read: {kind}"),
        SkipReason::CaseConflict => {
            "the same name as another file except for the case of the letters".to_string()
        }
        _ => "was not taken".to_string(),
    }
}

fn skipped_line(skipped: &Skipped) -> String {
    format!(
        "skipped: {}/{}: {}",
        skipped.kind.folder_name(),
        skipped.path.display(),
        skip_reason_text(&skipped.reason)
    )
}

/// What a scan found, one line for each Asset or file. `describe` gives the
/// words for an ID (its ID and its path).
fn format_scan_report(report: &ScanReport, describe: &dyn Fn(&AssetId) -> String) -> Vec<String> {
    let mut lines = Vec::new();
    for id in &report.added {
        lines.push(format!("added: {}", describe(id)));
    }
    for id in &report.missing {
        lines.push(format!("missing: {}", describe(id)));
    }
    for id in &report.restored {
        lines.push(format!("restored: {}", describe(id)));
    }
    for (id, failure) in &report.unreadable {
        lines.push(format!("unreadable: {}: {failure}", describe(id)));
    }
    for skipped in &report.skipped {
        lines.push(skipped_line(skipped));
    }
    lines.iter().map(|line| message_line(line)).collect()
}

/// The result of a scan, for stdout.
fn format_scan_summary(report: &ScanReport) -> String {
    format!(
        "scanned: {} added, {} missing, {} restored, {} unreadable, {} skipped",
        report.added.len(),
        report.missing.len(),
        report.restored.len(),
        report.unreadable.len(),
        report.skipped.len()
    )
}

/// The result of an import, for stdout: the word, the ID and the path of the
/// copy, with tabs between.
fn format_import_result(word: &str, id: &AssetId, path: Option<&Path>) -> String {
    let path = path.map(|p| p.display().to_string()).unwrap_or_default();
    format!("{word}\t{id}\t{}", one_line(&path))
}

/// The line for a refused or failed import, with a way out where there is one.
fn import_error_line(error: &ImportError) -> String {
    let hint = match error {
        ImportError::TooLongForSe { .. } => " (to register it as a song, use --kind bgm)",
        ImportError::AlreadyInLibrary => " (run `library scan` to pick it up)",
        _ => "",
    };
    message_line(&format!("error: {error}{hint}"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use std::time::Duration;

    use ink_core::{AssetFormat, PathProblem};

    use super::*;
    use crate::cli::KindArg;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A fresh folder under the system temp dir, unique per test.
    fn test_dir(name: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ink-mixer-library-cmd-test-{}-{n}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn id(text: &str) -> AssetId {
        text.parse().unwrap()
    }

    fn info(frames: Option<u64>, rate: u32) -> AssetInfo {
        AssetInfo {
            format: AssetFormat::Wav,
            sample_rate: rate,
            channels: 2,
            frames,
        }
    }

    // -- the folder ------------------------------------------------------------------

    #[test]
    fn the_given_folder_wins_over_the_default() {
        let given = Path::new("some/where");
        let default = Some(PathBuf::from("/data/inkmixer"));
        assert_eq!(
            resolve_root(Some(given), default.clone()),
            Ok(PathBuf::from("some/where"))
        );
        assert_eq!(
            resolve_root(Some(given), None),
            Ok(PathBuf::from("some/where"))
        );
        assert_eq!(
            resolve_root(None, default),
            Ok(PathBuf::from("/data/inkmixer"))
        );
    }

    #[test]
    fn without_a_folder_the_error_says_how_to_give_one() {
        let message = resolve_root(None, None).unwrap_err();
        assert!(message.contains("--library-dir"), "{message}");
    }

    #[test]
    fn the_default_folder_is_an_absolute_path_if_the_os_has_one() {
        if let Some(root) = library_root() {
            assert!(root.is_absolute(), "{}", root.display());
        }
    }

    // -- the text --------------------------------------------------------------------

    #[test]
    fn a_length_is_minutes_seconds_and_milliseconds() {
        let wav = |frames, rate| format_length(Some(&info(Some(frames), rate)));
        assert_eq!(wav(0, 48_000), "0:00.000");
        assert_eq!(wav(24_000, 48_000), "0:00.500");
        assert_eq!(wav(48_000, 48_000), "0:01.000");
        assert_eq!(wav(44_100 * 65 + 11_025, 44_100), "1:05.250");
        assert_eq!(wav(48_000 * 3599, 48_000), "59:59.000");
        assert_eq!(wav(48_000 * 3600, 48_000), "1:00:00.000");
        assert_eq!(wav(48_000 * 36_000 + 24_000, 48_000), "10:00:00.500");
        // Cut, not rounded: 1/3 of a second is .333, 2/3 is .666.
        assert_eq!(wav(1, 3), "0:00.333");
        assert_eq!(wav(2, 3), "0:00.666");
    }

    #[test]
    fn a_length_that_is_not_known_is_told_apart_from_a_file_never_read() {
        assert_eq!(format_length(None), "-", "never read");
        assert_eq!(
            format_length(Some(&info(None, 48_000))),
            "?",
            "read, no length"
        );
        assert_eq!(format_length(Some(&info(Some(10), 0))), "?");
    }

    /// A library with one sound effect, `se-0007`, whose file is there. The
    /// name is the text of the `name` in the index (TOML escapes allowed).
    fn library_with_a_sound(name_in_toml: &str) -> Library {
        let root = test_dir("one-sound");
        fs::create_dir_all(root.join("sounds/se")).unwrap();
        fs::write(root.join("sounds/se/clap.wav"), b"x").unwrap();
        fs::write(
            root.join("library.toml"),
            format!(
                "version = 1\nnext_id = 8\n\n[[asset]]\nid = \"se-0007\"\nkind = \"se\"\n\
                 name = \"{name_in_toml}\"\npath = \"clap.wav\"\n\n\
                 [asset.info]\nformat = \"wav\"\nsample_rate = 48000\nchannels = 2\nframes = 48000\n"
            ),
        )
        .unwrap();
        Library::open(&root).unwrap().library
    }

    #[test]
    fn a_line_of_the_list_has_six_columns_with_tabs() {
        let library = library_with_a_sound("clap");
        let asset = library.get(&id("se-0007")).unwrap();
        let line = format_list_line(asset, Some(Path::new("/lib/sounds/se/clap.wav")));
        let columns: Vec<&str> = line.split('\t').collect();
        assert_eq!(
            columns,
            [
                "clap",
                "se-0007",
                "se",
                "available",
                "0:01.000",
                "/lib/sounds/se/clap.wav"
            ]
        );
    }

    #[test]
    fn a_name_cannot_break_the_columns() {
        let library = library_with_a_sound("two\\tcolumns\\nand a line");
        let asset = library.get(&id("se-0007")).unwrap();
        assert!(asset.name().contains('\t') && asset.name().contains('\n'));
        let line = format_list_line(asset, Some(Path::new("/p")));
        assert_eq!(line.split('\t').count(), 6, "{line:?}");
        assert!(!line.contains('\n'));
        assert!(line.starts_with("two columns and a line\t"), "{line:?}");
    }

    #[test]
    fn a_message_of_a_library_becomes_one_line_with_single_spaces() {
        let toml = "TOML parse error at line 1, column 8\n  |\n1 | garbage\n  |        ^\nkey with no value\n";
        assert_eq!(
            message_line(toml),
            "TOML parse error at line 1, column 8 | 1 | garbage | ^ key with no value"
        );
        assert_eq!(message_line("  a\tb \r\n c  "), "a b c");
        // A column of the list keeps its spaces.
        assert_eq!(one_line("a  b"), "a  b");
    }

    #[test]
    fn the_words_of_the_states_and_kinds() {
        assert_eq!(state_word(AssetState::Available), "available");
        assert_eq!(state_word(AssetState::Missing), "missing");
        assert_eq!(state_word(AssetState::Unreadable), "unreadable");
        assert_eq!(AssetKind::Bgm.folder_name(), "bgm");
    }

    #[test]
    fn no_warning_is_no_line_and_warnings_come_with_what_they_mean() {
        assert!(format_warnings(&[]).is_empty());
        let warnings = [IndexWarning::DuplicateId { id: id("se-0002") }];
        let lines = format_warnings(&warnings);
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].starts_with("warning: library.toml: "),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].contains("lost from the index at the next save"),
            "{}",
            lines[1]
        );
        assert!(lines[1].contains("1 problem"), "{}", lines[1]);
    }

    #[test]
    fn a_broken_index_is_told_with_how_to_move_it_by_hand() {
        let root = Path::new("/lib");
        let error = LibraryError::Index(ink_core::IndexError::MissingVersion);
        let lines = open_error_lines(root, &error);
        assert_eq!(lines.len(), 2);
        let index = root.join("library.toml");
        assert!(
            lines[0].starts_with(&format!("error: {} cannot be used", index.display())),
            "{}",
            lines[0]
        );
        assert!(lines[1].contains("library.toml.broken"), "{}", lines[1]);
        assert!(lines[1].contains("do not delete"), "{}", lines[1]);
        assert!(lines[1].contains("new IDs"), "{}", lines[1]);
        assert!(lines[1].contains("trigger words"), "{}", lines[1]);

        // Another failure has no advice about the index.
        let io = LibraryError::Io {
            path: PathBuf::from("/lib"),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        assert_eq!(open_error_lines(root, &io).len(), 1);
    }

    #[test]
    fn a_scan_report_has_a_line_for_each_thing_it_found() {
        let report = ScanReport {
            added: vec![id("bgm-0003")],
            missing: vec![id("se-0002")],
            restored: vec![id("bgm-0001")],
            unreadable: vec![(
                id("bgm-0004"),
                ink_core::ProbeFailure::NotAudio("broken".to_string()),
            )],
            skipped: vec![
                Skipped {
                    kind: AssetKind::Se,
                    path: PathBuf::from("notes.txt"),
                    reason: SkipReason::UnsupportedExtension,
                },
                Skipped {
                    kind: AssetKind::Bgm,
                    path: PathBuf::from("CON.wav"),
                    reason: SkipReason::UnsafeName(PathProblem::ReservedName),
                },
            ],
        };
        let describe = |id: &AssetId| format!("{id} <{id}>");
        let lines = format_scan_report(&report, &describe);
        assert_eq!(
            lines,
            [
                "added: bgm-0003 <bgm-0003>",
                "missing: se-0002 <se-0002>",
                "restored: bgm-0001 <bgm-0001>",
                "unreadable: bgm-0004 <bgm-0004>: not a sound that can be played: broken",
                "skipped: se/notes.txt: the extension is not wav or mp3",
                "skipped: bgm/CON.wav: the name cannot be used: a part of the path is a name kept by Windows",
            ]
        );
        assert_eq!(
            format_scan_summary(&report),
            "scanned: 1 added, 1 missing, 1 restored, 1 unreadable, 2 skipped"
        );
        assert_eq!(
            format_scan_summary(&ScanReport::default()),
            "scanned: 0 added, 0 missing, 0 restored, 0 unreadable, 0 skipped"
        );
        assert!(format_scan_report(&ScanReport::default(), &describe).is_empty());
    }

    #[test]
    fn every_reason_of_a_skip_has_words() {
        for reason in [
            SkipReason::UnsupportedExtension,
            SkipReason::NameNotUtf8,
            SkipReason::UnsafeName(PathProblem::Empty),
            SkipReason::TooDeep,
            SkipReason::Symlink,
            SkipReason::NotReadable(io::ErrorKind::PermissionDenied),
            SkipReason::CaseConflict,
        ] {
            let text = skip_reason_text(&reason);
            assert!(!text.is_empty() && !text.contains('\n'), "{reason:?}");
            assert_ne!(text, "was not taken", "{reason:?} has no words of its own");
        }
    }

    #[test]
    fn an_import_error_is_one_line_and_some_have_a_way_out() {
        let too_long = ImportError::TooLongForSe {
            limit: Duration::from_secs(60),
            length: Duration::from_secs(61),
        };
        let line = import_error_line(&too_long);
        assert!(line.starts_with("error: "), "{line}");
        assert!(line.contains("--kind bgm"), "{line}");
        assert!(import_error_line(&ImportError::AlreadyInLibrary).contains("library scan"));
        let plain = import_error_line(&ImportError::NotAFile);
        assert_eq!(plain, "error: the source is not a file");
        let multi = ImportError::NotAudio("two\nlines".to_string());
        assert!(!import_error_line(&multi).contains('\n'));
    }

    #[test]
    fn an_import_result_is_the_word_the_id_and_the_path() {
        assert_eq!(
            format_import_result("imported", &id("bgm-0001"), Some(Path::new("/lib/a.wav"))),
            "imported\tbgm-0001\t/lib/a.wav"
        );
        assert_eq!(
            format_import_result("already there", &id("se-0002"), None),
            "already there\tse-0002\t"
        );
    }

    #[test]
    fn the_exit_code_is_0_for_success_and_1_for_failure() {
        assert_eq!(
            format!("{:?}", exit_code(true)),
            format!("{:?}", ExitCode::SUCCESS)
        );
        assert_eq!(
            format!("{:?}", exit_code(false)),
            format!("{:?}", ExitCode::FAILURE)
        );
        assert_ne!(
            format!("{:?}", exit_code(true)),
            format!("{:?}", exit_code(false))
        );
    }

    // -- through the commands, on a folder of the test ---------------------------------

    /// A WAV file of `frames` frames of silence (16 bit).
    fn wav(sample_rate: u32, channels: u16, frames: u32) -> Vec<u8> {
        let block_align = channels * 2;
        let data_len = frames * u32::from(block_align);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&(sample_rate * u32::from(block_align)).to_le_bytes());
        bytes.extend_from_slice(&block_align.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        bytes.resize(bytes.len() + data_len as usize, 0);
        bytes
    }

    /// What a command wrote.
    struct Ran {
        ok: bool,
        out: String,
        err: String,
    }

    impl Ran {
        fn out_lines(&self) -> Vec<&str> {
            self.out.lines().collect()
        }
        fn err_lines(&self) -> Vec<&str> {
            self.err.lines().collect()
        }
    }

    /// Runs `action` on the folder `root` as `--library-dir` does, with the real
    /// reader of sound files.
    fn run_action(root: &Path, action: LibraryAction) -> Ran {
        run_with_default(Some(root), None, action)
    }

    fn run_with_default(
        given: Option<&Path>,
        default_root: Option<PathBuf>,
        action: LibraryAction,
    ) -> Ran {
        let args = LibraryArgs {
            library_dir: given.map(Path::to_path_buf),
            action,
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        let ok = execute(&args, default_root, &asset_info, &mut out, &mut err);
        Ran {
            ok,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }

    fn import_action(file: &Path, kind: KindArg) -> LibraryAction {
        LibraryAction::Import(ImportArgs {
            file: file.to_path_buf(),
            kind,
        })
    }

    #[test]
    fn path_prints_the_folder_and_makes_nothing() {
        let parent = test_dir("path");
        let root = parent.join("not-made-yet");
        let ran = run_action(&root, LibraryAction::Path);
        assert!(ran.ok);
        assert_eq!(ran.out, format!("{}\n", root.display()));
        assert!(ran.err.is_empty());
        assert!(!root.exists(), "path must not make the folder");
    }

    #[test]
    fn path_makes_a_relative_folder_absolute() {
        let ran = run_action(Path::new("some/relative"), LibraryAction::Path);
        assert!(ran.ok);
        let printed = PathBuf::from(ran.out.trim_end());
        assert!(printed.is_absolute(), "{}", printed.display());
        assert!(printed.ends_with("some/relative"));
    }

    #[test]
    fn without_any_folder_every_action_fails_and_says_so() {
        for action in [
            LibraryAction::Path,
            LibraryAction::List,
            LibraryAction::Scan,
        ] {
            let ran = run_with_default(None, None, action);
            assert!(!ran.ok);
            assert!(ran.out.is_empty());
            assert_eq!(ran.err_lines().len(), 1);
            assert!(ran.err.contains("--library-dir"), "{}", ran.err);
        }
    }

    #[test]
    fn the_default_folder_is_used_when_none_is_given() {
        let root = test_dir("default");
        let ran = run_with_default(None, Some(root.clone()), LibraryAction::Path);
        assert!(ran.ok);
        assert_eq!(ran.out, format!("{}\n", root.display()));
    }

    #[test]
    fn import_then_list_then_scan_work_through_the_commands() {
        let root = test_dir("flow");
        let sources = test_dir("flow-sources");
        let song = sources.join("音楽 テスト.wav");
        fs::write(&song, wav(48_000, 2, 72_000)).unwrap();
        let clap = sources.join("clap.wav");
        fs::write(&clap, wav(8_000, 1, 4_000)).unwrap();

        // import
        let ran = run_action(&root, import_action(&song, KindArg::Bgm));
        assert!(ran.ok, "{}", ran.err);
        assert!(ran.err.is_empty(), "{}", ran.err);
        let copy = root.join("sounds").join("bgm").join("音楽 テスト.wav");
        assert_eq!(ran.out, format!("imported\tbgm-0001\t{}\n", copy.display()));
        assert!(copy.is_file());
        let ran = run_action(&root, import_action(&clap, KindArg::Se));
        assert!(ran.ok, "{}", ran.err);
        assert!(ran.out.starts_with("imported\tse-0002\t"), "{}", ran.out);

        // the same file again
        let ran = run_action(&root, import_action(&song, KindArg::Bgm));
        assert!(ran.ok);
        assert_eq!(
            ran.out,
            format!("already there\tbgm-0001\t{}\n", copy.display())
        );

        // list: name, ID, kind, state, length, path
        let ran = run_action(&root, LibraryAction::List);
        assert!(ran.ok);
        assert!(ran.err.is_empty(), "{}", ran.err);
        let se_copy = root.join("sounds").join("se").join("clap.wav");
        assert_eq!(
            ran.out_lines(),
            [
                format!(
                    "音楽 テスト\tbgm-0001\tbgm\tavailable\t0:01.500\t{}",
                    copy.display()
                ),
                format!(
                    "clap\tse-0002\tse\tavailable\t0:00.500\t{}",
                    se_copy.display()
                ),
            ]
        );

        // a file put in by hand, a file that is not a sound, and one that goes away
        fs::write(root.join("sounds/bgm/hand.wav"), wav(44_100, 2, 44_100)).unwrap();
        fs::write(root.join("sounds/bgm/broken.wav"), b"not a sound").unwrap();
        fs::write(root.join("sounds/bgm/notes.txt"), b"text").unwrap();
        fs::remove_file(&se_copy).unwrap();
        let ran = run_action(&root, LibraryAction::Scan);
        assert!(ran.ok, "{}", ran.err);
        assert_eq!(
            ran.out,
            "scanned: 2 added, 1 missing, 0 restored, 1 unreadable, 1 skipped\n"
        );
        let lines = ran.err_lines();
        assert_eq!(lines[0], "added: bgm-0003 broken.wav");
        assert_eq!(lines[1], "added: bgm-0004 hand.wav");
        assert_eq!(lines[2], "missing: se-0002 clap.wav");
        assert!(
            lines[3].starts_with("unreadable: bgm-0003 broken.wav: "),
            "{}",
            lines[3]
        );
        assert_eq!(
            lines[4],
            "skipped: bgm/notes.txt: the extension is not wav or mp3"
        );
        assert_eq!(lines.len(), 5);

        // The next run starts from the files again, and still tells what is
        // missing or cannot be read.
        let ran = run_action(&root, LibraryAction::Scan);
        assert!(ran.ok, "{}", ran.err);
        assert_eq!(
            ran.out,
            "scanned: 0 added, 1 missing, 0 restored, 1 unreadable, 1 skipped\n"
        );
        let lines = ran.err_lines();
        assert_eq!(lines[0], "missing: se-0002 clap.wav");
        assert!(
            lines[1].starts_with("unreadable: bgm-0003 broken.wav: "),
            "{}",
            lines[1]
        );
        assert_eq!(
            lines[2],
            "skipped: bgm/notes.txt: the extension is not wav or mp3"
        );
        assert_eq!(lines.len(), 3);

        // list again: states and lengths (never-read files show `-`)
        let ran = run_action(&root, LibraryAction::List);
        assert!(ran.ok);
        let out = ran.out_lines();
        assert_eq!(out.len(), 4);
        assert!(
            out[1].starts_with("clap\tse-0002\tse\tmissing\t0:00.500\t"),
            "{}",
            out[1]
        );
        assert!(
            out[2].starts_with("broken\tbgm-0003\tbgm\tunreadable\t-\t"),
            "{}",
            out[2]
        );
        assert!(
            out[3].starts_with("hand\tbgm-0004\tbgm\tavailable\t0:01.000\t"),
            "{}",
            out[3]
        );
    }

    #[test]
    fn an_empty_library_lists_nothing_and_says_what_to_do() {
        let root = test_dir("empty");
        let ran = run_action(&root, LibraryAction::List);
        assert!(ran.ok);
        assert!(ran.out.is_empty());
        assert_eq!(ran.err_lines().len(), 1);
        assert!(
            ran.err.starts_with("note: the library has no sounds"),
            "{}",
            ran.err
        );
        for folder in [
            root.join("sounds").join("bgm"),
            root.join("sounds").join("se"),
        ] {
            assert!(
                ran.err.contains(&folder.display().to_string()),
                "{}",
                ran.err
            );
        }
    }

    #[test]
    fn a_file_that_cannot_be_imported_fails_with_one_line() {
        let root = test_dir("refused");
        let sources = test_dir("refused-sources");
        let text = sources.join("text.wav");
        fs::write(&text, b"not a sound").unwrap();
        let long = sources.join("long.wav");
        fs::write(&long, wav(8_000, 1, 8_000 * 61)).unwrap();

        for (file, kind, expected) in [
            (&text, KindArg::Se, "error: not a sound that can be played"),
            (
                &long,
                KindArg::Se,
                "error: a sound effect can be 60 seconds at most",
            ),
            (
                &sources.join("missing.wav"),
                KindArg::Bgm,
                "error: cannot read the source",
            ),
        ] {
            let ran = run_action(&root, import_action(file, kind));
            assert!(!ran.ok, "{}", file.display());
            assert!(ran.out.is_empty());
            assert_eq!(ran.err_lines().len(), 1, "{}", ran.err);
            assert!(ran.err.starts_with(expected), "{}", ran.err);
        }
        assert!(
            run_action(&root, import_action(&long, KindArg::Se))
                .err
                .contains("--kind bgm")
        );
        // The same file is fine as a song.
        assert!(run_action(&root, import_action(&long, KindArg::Bgm)).ok);
    }

    #[test]
    fn a_broken_index_is_never_overwritten_and_nothing_is_made() {
        let root = test_dir("broken");
        let index = root.join("library.toml");
        let sources = test_dir("broken-sources");
        let song = sources.join("song.wav");
        fs::write(&song, wav(48_000, 2, 4_800)).unwrap();

        for text in ["this is not an index", "", "version = 99\n"] {
            fs::write(&index, text).unwrap();
            for action in [
                LibraryAction::List,
                LibraryAction::Scan,
                import_action(&song, KindArg::Bgm),
            ] {
                let ran = run_action(&root, action);
                assert!(!ran.ok, "{text:?}");
                assert!(ran.out.is_empty(), "{text:?}: {}", ran.out);
                let lines = ran.err_lines();
                assert_eq!(lines.len(), 2, "{text:?}: {}", ran.err);
                assert!(
                    lines[0].contains("library.toml cannot be used"),
                    "{}",
                    lines[0]
                );
                assert!(
                    lines[1].starts_with("hint: move library.toml"),
                    "{}",
                    lines[1]
                );
                assert_eq!(
                    fs::read_to_string(&index).unwrap(),
                    text,
                    "{text:?} was changed"
                );
                assert!(!root.join("sounds").exists(), "{text:?}: folders were made");
                assert!(
                    !root.join("library.toml.broken").exists(),
                    "nothing is moved"
                );
            }
        }
        // `path` does not need the index.
        assert!(run_action(&root, LibraryAction::Path).ok);
    }

    #[test]
    fn warnings_of_the_index_are_told_and_list_does_not_rewrite_the_file() {
        let root = test_dir("warnings");
        let index = root.join("library.toml");
        let text = "version = 1\nnext_id = 5\n\n\
                    [[asset]]\nid = \"se-0001\"\nkind = \"se\"\npath = \"ok.wav\"\n\n\
                    [[asset]]\nid = \"se-0002\"\nkind = \"se\"\npath = \"../outside.wav\"\n";
        fs::write(&index, text).unwrap();
        let ran = run_action(&root, LibraryAction::List);
        assert!(ran.ok, "{}", ran.err);
        let lines = ran.err_lines();
        assert!(
            lines[0].starts_with("warning: library.toml: "),
            "{}",
            lines[0]
        );
        assert!(lines[0].contains("se-0002"), "{}", lines[0]);
        assert!(
            lines[1].contains("lost from the index at the next save"),
            "{}",
            lines[1]
        );
        assert_eq!(ran.out_lines().len(), 1);
        assert!(ran.out.starts_with("ok\tse-0001\tse\t"), "{}", ran.out);
        assert_eq!(
            fs::read_to_string(&index).unwrap(),
            text,
            "list must not write"
        );
    }
}
