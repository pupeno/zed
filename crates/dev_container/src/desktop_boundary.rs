//! Guards the indistinguishability invariant of the host-side Dev Container path.
//!
//! Reopening a remote source project in its Dev Container must produce the same
//! result as a Zed running natively on that project host. So the desktop's
//! operating system, account, environment, and path conventions must not be
//! observable in the outcome, and nothing in this crate may reach around
//! [`remote::ProjectHost`] to ask the desktop a question the project host owns.
//!
//! This is a source guard rather than a list of known sites: it scans this
//! crate's own production code for the *classes* of desktop state that have
//! leaked before, so a newly written leak of the same class fails here without
//! anyone remembering to sweep for it. The four leaks that motivated it — a
//! `#[cfg(target_os = "windows")]` remap predicate, a desktop shell environment,
//! a desktop temporary root, and `Path::is_absolute()` applied to a Linux path
//! on a Windows desktop — are each an instance of one of the patterns below.
//!
//! The path classes are guarded at the point a desktop path *enters* the crate
//! rather than at each predicate applied to one. `Path::is_absolute()` on a
//! Linux path is only reachable from a `Path` value, and a `Path` value can only
//! arrive by naming the type or by converting to it, both of which are findings.
//! Guarding the entry means a predicate nobody thought to add to a needle list
//! still cannot be reached.
//!
//! Scope is `crates/dev_container/src` only. `remote::ProjectHost` is where
//! desktop-local facts are legitimately *defined* — a local project host reads
//! the desktop's temporary directory because for that host they are the same
//! machine. This crate is the consumer, and consumers must go through the trait.
//!
//! # Escaping the guard
//!
//! Some desktop state genuinely belongs here: feature and template tarballs are
//! fetched over HTTP by the desktop and unpacked with the desktop's filesystem
//! before being staged onto the project host. Write the reason in a comment on
//! or just above the line, including the word `desktop-local`, and the guard
//! accepts it. The point is that the next reader can tell intent from oversight.

use std::{fs, path::Path};

/// A comment carrying this word marks a deliberate crossing of the boundary.
const MARKER: &str = "desktop-local";

/// How far above a finding a justifying comment may sit. Wide enough for a doc
/// comment on the enclosing item, narrow enough that an unrelated comment
/// further up the file cannot silence something.
const JUSTIFICATION_WINDOW: usize = 10;

/// This module is test-only and necessarily names every pattern it looks for.
const SELF: &str = "desktop_boundary.rs";

#[derive(Debug, PartialEq, Eq)]
struct Finding {
    file: String,
    line_number: usize,
    class: &'static str,
    line: String,
}

/// The desktop facts that must not decide a project-host outcome.
///
/// Matched against a line with its comments removed, so prose describing a path
/// is not mistaken for code interpreting one.
fn leak_class(code: &str) -> Option<&'static str> {
    const NEEDLES: &[(&str, &str)] = &[
        ("cfg(target_os", "desktop build-time platform condition"),
        ("cfg(windows", "desktop build-time platform condition"),
        ("cfg(unix", "desktop build-time platform condition"),
        ("cfg!(", "desktop build-time platform condition"),
        ("env::consts", "desktop build-time platform condition"),
        ("env::var", "desktop process environment"),
        ("env::set_var", "desktop process environment"),
        ("temp_dir(", "desktop temporary directory"),
        ("home_dir(", "desktop home directory"),
        // Qualified, because this crate's own `HostCommand::current_dir` sets a
        // working directory on the project host, which is the correct thing.
        ("env::current_dir(", "desktop working directory"),
        ("env::current_exe(", "desktop process location"),
        ("PathStyle::local(", "desktop path style"),
        // The two ways to obtain a desktop path without naming its type.
        ("to_path_buf(", "desktop-native path type"),
        ("as_std_path(", "desktop-native path type"),
    ];
    for (needle, class) in NEEDLES {
        if code.contains(needle) {
            return Some(class);
        }
    }
    // `Path` and `PathBuf` answer absoluteness, root, prefix, and separator
    // questions with the desktop's rules. `HostPathBuf`, `RelPath`, and
    // `PathStyle` are deliberately not matched: they carry the owning machine's
    // rules with them.
    for identifier in ["Path", "PathBuf"] {
        if contains_identifier(code, identifier) {
            return Some("desktop-native path type");
        }
    }
    None
}

/// Whether `haystack` contains `identifier` as a whole Rust identifier, so that
/// `HostPathBuf` and `RelPath` do not match `PathBuf` and `Path`.
fn contains_identifier(haystack: &str, identifier: &str) -> bool {
    let is_word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let bytes = haystack.as_bytes();
    haystack.match_indices(identifier).any(|(start, _)| {
        let end = start + identifier.len();
        let before_is_word = start > 0 && is_word(bytes[start - 1]);
        let after_is_word = end < bytes.len() && is_word(bytes[end]);
        !before_is_word && !after_is_word
    })
}

/// Splits a line into the code it executes and the comment explaining it.
///
/// `://` is left alone so a URL in a string literal is not read as a comment.
fn split_code_and_comment(line: &str) -> (&str, &str) {
    let bytes = line.as_bytes();
    let mut index = 0;
    while index + 1 < bytes.len() {
        if bytes[index] == b'/' && bytes[index + 1] == b'/' {
            if index > 0 && bytes[index - 1] == b':' {
                index += 2;
                continue;
            }
            return (&line[..index], &line[index..]);
        }
        index += 1;
    }
    (line, "")
}

/// The production part of a source file: everything before its `#[cfg(test)]`
/// module. A bare `#[cfg(test)]` on an import is not the boundary.
fn production_lines(source: &str) -> Vec<&str> {
    let lines: Vec<&str> = source.lines().collect();
    for (index, line) in lines.iter().enumerate() {
        if line.trim() != "#[cfg(test)]" {
            continue;
        }
        let follows_a_module = lines[index + 1..]
            .iter()
            .find(|line| !line.trim().is_empty())
            .is_some_and(|line| {
                let line = line.trim_start();
                line.starts_with("mod ")
                    || line.starts_with("pub mod ")
                    || line.starts_with("pub(crate) mod ")
            });
        if follows_a_module {
            return lines[..index].to_vec();
        }
    }
    lines
}

/// Whether a line only names a type rather than deciding anything with it. An
/// import is not a leak; the uses it enables are what this guard reads.
fn is_import(code: &str) -> bool {
    let code = code.trim_start();
    code.starts_with("use ") || code.starts_with("pub use ")
}

fn findings_in(file: &str, source: &str) -> Vec<Finding> {
    let lines = production_lines(source);
    let mut findings = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let (code, _) = split_code_and_comment(line);
        if is_import(code) {
            continue;
        }
        let Some(class) = leak_class(code) else {
            continue;
        };
        let window_start = index.saturating_sub(JUSTIFICATION_WINDOW);
        let justified = lines[window_start..=index].iter().any(|line| {
            split_code_and_comment(line)
                .1
                .to_lowercase()
                .contains(MARKER)
        });
        if !justified {
            findings.push(Finding {
                file: file.to_string(),
                line_number: index + 1,
                class,
                line: line.trim().to_string(),
            });
        }
    }
    findings
}

#[test]
fn the_host_side_dev_container_path_carries_no_unjustified_desktop_state() {
    let source_directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut sources: Vec<_> = fs::read_dir(&source_directory)
        .expect("the crate's own source directory is readable")
        .map(|entry| entry.expect("a readable directory entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .filter(|path| path.file_name().is_some_and(|name| name != SELF))
        .collect();
    sources.sort();
    assert!(
        !sources.is_empty(),
        "found no sources to scan under {}",
        source_directory.display()
    );

    let findings: Vec<Finding> = sources
        .iter()
        .flat_map(|path| {
            let file = path
                .file_name()
                .expect("a source file has a name")
                .to_string_lossy()
                .into_owned();
            let source = fs::read_to_string(path).expect("a source file is readable");
            findings_in(&file, &source)
        })
        .collect();

    if findings.is_empty() {
        return;
    }
    let report = findings
        .iter()
        .map(|finding| {
            format!(
                "  {}:{} — {}\n      {}",
                finding.file, finding.line_number, finding.class, finding.line
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    panic!(
        "the host-side Dev Container path reads desktop state that a project host owns:\n\
         {report}\n\n\
         Route the decision through `remote::ProjectHost`: ask it for the platform, the \n\
         environment, the temporary root, or a `HostPathBuf` instead of asking the desktop. \n\
         If the value really is a desktop-local asset staged onto the host, say so in a \n\
         comment on or above the line containing the word `{MARKER}`."
    );
}

#[cfg(test)]
mod tests {
    use super::{Finding, findings_in};

    fn classes(source: &str) -> Vec<&'static str> {
        findings_in("sample.rs", source)
            .iter()
            .map(|finding: &Finding| finding.class)
            .collect()
    }

    #[test]
    fn a_desktop_build_time_platform_condition_is_a_finding() {
        assert_eq!(
            classes("#[cfg(target_os = \"windows\")]\nfn remaps() -> bool { false }\n"),
            ["desktop build-time platform condition"]
        );
        assert_eq!(
            classes("let remaps = !cfg!(windows);\n"),
            ["desktop build-time platform condition"]
        );
    }

    #[test]
    fn a_desktop_environment_or_temporary_directory_read_is_a_finding() {
        assert_eq!(
            classes("let root = std::env::temp_dir();\n"),
            ["desktop temporary directory"]
        );
        assert_eq!(
            classes("let user = std::env::var(\"USER\");\n"),
            ["desktop process environment"]
        );
    }

    #[test]
    fn a_desktop_native_path_type_is_a_finding() {
        assert_eq!(
            classes("fn root(&self) -> PathBuf { todo!() }\n"),
            ["desktop-native path type"]
        );
        assert_eq!(
            classes("if Path::new(context).is_absolute() { return; }\n"),
            ["desktop-native path type"]
        );
        assert_eq!(
            classes("fn stage(source: &Path) {}\n"),
            ["desktop-native path type"]
        );
    }

    #[test]
    fn host_semantic_types_are_not_findings() {
        assert!(
            classes(
                "fn root(&self) -> HostPathBuf { todo!() }\n\
                 fn config(&self) -> Arc<RelPath> { todo!() }\n\
                 fn style(&self) -> PathStyle { self.host.path_style() }\n\
                 let remaps = !self.host.platform().is_windows();\n"
            )
            .is_empty()
        );
    }

    #[test]
    fn the_desktop_path_style_is_a_finding_even_though_path_style_is_not() {
        assert_eq!(
            classes("let style = PathStyle::local();\n"),
            ["desktop path style"]
        );
    }

    #[test]
    fn prose_about_paths_is_not_a_finding() {
        assert!(classes("/// Path to the generated Dockerfile.\nfn generated() {}\n").is_empty());
    }

    #[test]
    fn converting_to_a_desktop_path_without_naming_the_type_is_a_finding() {
        assert_eq!(
            classes("let root = self.host.source_root().to_path_buf();\n"),
            ["desktop-native path type"]
        );
        assert_eq!(
            classes("let root = config.config_path.as_std_path();\n"),
            ["desktop-native path type"]
        );
    }

    #[test]
    fn an_import_is_not_a_finding() {
        assert!(classes("use std::path::{Path, PathBuf};\n").is_empty());
    }

    #[test]
    fn a_justifying_comment_silences_a_finding() {
        assert!(
            classes(
                "// Desktop-local: the tarball is fetched by the desktop.\n\
                 let staging = std::env::temp_dir();\n"
            )
            .is_empty()
        );
        assert!(classes("let staging = std::env::temp_dir(); // desktop-local\n").is_empty());
    }

    #[test]
    fn a_justification_does_not_reach_past_its_window() {
        let source = format!(
            "// Desktop-local: about something else entirely.\n{}let root = std::env::temp_dir();\n",
            "\n".repeat(11)
        );
        assert_eq!(classes(&source), ["desktop temporary directory"]);
    }

    #[test]
    fn a_string_literal_cannot_justify_a_finding() {
        assert_eq!(
            classes("let label = \"desktop-local\"; let root = std::env::temp_dir();\n"),
            ["desktop temporary directory"]
        );
    }

    #[test]
    fn a_url_in_a_string_literal_does_not_hide_the_rest_of_the_line() {
        assert_eq!(
            classes("let url = \"https://example.test\"; let root = std::env::temp_dir();\n"),
            ["desktop temporary directory"]
        );
    }

    #[test]
    fn test_modules_are_not_scanned() {
        assert!(
            classes(
                "fn production() {}\n\
                 #[cfg(test)]\n\
                 mod test {\n    let root = std::env::temp_dir();\n}\n"
            )
            .is_empty()
        );
    }

    #[test]
    fn a_test_only_import_is_not_the_test_module_boundary() {
        assert_eq!(
            classes(
                "#[cfg(test)]\n\
                 use util::paths::PathStyle;\n\
                 fn production() { let root = std::env::temp_dir(); }\n"
            ),
            ["desktop temporary directory"]
        );
    }
}
