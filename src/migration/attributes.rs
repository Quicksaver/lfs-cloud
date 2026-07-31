// This file is included by `mod.rs` so the migration API remains in one module.

fn parse_git_check_attr_filter_stdout(
    stdout: &[u8],
    command_name: &str,
) -> MigrationResult<Vec<PathBuf>> {
    parse_lfs_filter_attribute_paths(stdout)
        .map_err(|error| migration_git_path_output_error(error, command_name))
}

fn git_check_attr_filter(worktree_root: &Path, tracked_paths: Vec<u8>) -> MigrationResult<Output> {
    git_check_attr_filter_with_source(worktree_root, tracked_paths, None)
}

fn git_check_attr_filter_with_source(
    worktree_root: &Path,
    mut tracked_paths: Vec<u8>,
    source: Option<&str>,
) -> MigrationResult<Output> {
    if !tracked_paths.ends_with(b"\0") {
        tracked_paths.push(b'\0');
    }

    let mut args = vec![
        OsString::from("check-attr"),
        OsString::from("-z"),
        OsString::from("--stdin"),
    ];
    let command_name = git_check_attr_filter_command_name(source);
    if let Some(source) = source {
        args.push(OsString::from(format!("--source={source}")));
    }
    args.push(OsString::from("filter"));

    let mut child = read_only_git_command()
        .args(&args)
        .current_dir(worktree_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| MigrationError::Io {
            context: format!("failed to start {command_name}"),
            source,
        })?;

    let mut stdin = child.stdin.take().ok_or_else(|| MigrationError::Io {
        context: "git check-attr stdin was not piped".to_owned(),
        source: io::Error::other("git check-attr stdin was not piped"),
    })?;
    let stdout = child.stdout.take().ok_or_else(|| MigrationError::Io {
        context: "git check-attr stdout was not piped".to_owned(),
        source: io::Error::other("git check-attr stdout was not piped"),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| MigrationError::Io {
        context: "git check-attr stderr was not piped".to_owned(),
        source: io::Error::other("git check-attr stderr was not piped"),
    })?;
    let stdin_writer = std::thread::spawn(move || {
        let write_result = stdin.write_all(&tracked_paths);
        drop(stdin);

        write_result
    });
    let stdout_reader = std::thread::spawn(move || {
        read_pipe_with_limit(stdout, MAX_CURRENT_CHECKOUT_ATTR_OUTPUT_BYTES)
    });
    let stderr_reader = std::thread::spawn(move || {
        read_pipe_with_limit(stderr, MAX_MIGRATION_GIT_OUTPUT_BYTES + 1)
    });

    let status = child.wait().map_err(|source| MigrationError::Io {
        context: format!("failed to wait for {command_name}"),
        source,
    })?;

    let write_result = stdin_writer.join().map_err(|_| MigrationError::Io {
        context: "git check-attr input writer panicked".to_owned(),
        source: io::Error::other("git check-attr input writer panicked"),
    })?;

    write_result.map_err(|source| MigrationError::Io {
        context: "failed to write git check-attr path input".to_owned(),
        source,
    })?;

    let stdout = stdout_reader
        .join()
        .map_err(|_| MigrationError::Io {
            context: "git check-attr stdout reader panicked".to_owned(),
            source: io::Error::other("git check-attr stdout reader panicked"),
        })?
        .map_err(|source| MigrationError::Io {
            context: "failed to read git check-attr stdout".to_owned(),
            source,
        })?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| MigrationError::Io {
            context: "git check-attr stderr reader panicked".to_owned(),
            source: io::Error::other("git check-attr stderr reader panicked"),
        })?
        .map_err(|source| MigrationError::Io {
            context: "failed to read git check-attr stderr".to_owned(),
            source,
        })?;

    if !status.success() {
        return Err(command_error(&command_name, status, &stderr.bytes));
    }

    if stdout.exceeded_limit {
        return Err(MigrationError::ExternalCommandOutput {
            command: command_name,
            message: SanitizedMessage::new("git returned too much attribute output"),
        });
    }

    Ok(Output {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
    })
}

struct PipeReadResult {
    bytes: Vec<u8>,
    exceeded_limit: bool,
}

fn read_pipe_with_limit(mut reader: impl Read, limit: usize) -> io::Result<PipeReadResult> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    let mut exceeded_limit = false;

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }

        let remaining = limit.saturating_sub(bytes.len());
        if remaining >= read {
            bytes.extend_from_slice(&buffer[..read]);
        } else {
            bytes.extend_from_slice(&buffer[..remaining]);
            exceeded_limit = true;
        }
    }

    Ok(PipeReadResult {
        bytes,
        exceeded_limit,
    })
}

fn git_check_attr_filter_command_name(source: Option<&str>) -> String {
    source.map_or_else(
        || "git check-attr -z --stdin filter".to_owned(),
        |source| format!("git check-attr -z --stdin --source={source} filter"),
    )
}

fn safe_git_relative_path(relative_path: &[u8], command: &str) -> MigrationResult<PathBuf> {
    parse_safe_git_relative_path(relative_path)
        .map_err(|error| migration_git_path_output_error(error, command))
}

fn migration_git_path_output_error(error: GitPathOutputError, command: &str) -> MigrationError {
    let message = match error {
        GitPathOutputError::MalformedAttributeOutput => "git returned malformed attribute output",
        #[cfg(not(unix))]
        GitPathOutputError::NonUtf8Path => "git returned non-UTF-8 path output",
        GitPathOutputError::PathOutsideWorktree => "git returned a path outside the worktree",
    };
    MigrationError::ExternalCommandOutput {
        command: command.to_owned(),
        message: SanitizedMessage::new(message),
    }
}

fn parse_lfs_patterns_from_attributes(
    contents: &str,
    source: PathBuf,
) -> Vec<GitLfsTrackedPattern> {
    let mut attribute_macros = BTreeMap::new();
    let mut patterns = Vec::new();

    for line in contents.lines() {
        if let Some(pattern) = parse_lfs_pattern_line(line, &source, &mut attribute_macros) {
            patterns.push(pattern);
        }
    }

    patterns
}

fn parse_lfs_pattern_line(
    line: &str,
    source: &Path,
    attribute_macros: &mut BTreeMap<String, Vec<String>>,
) -> Option<GitLfsTrackedPattern> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }

    let tokens = split_gitattributes_line(trimmed);
    let (pattern, attributes) = tokens.split_first()?;
    if let Some(macro_name) = pattern.strip_prefix("[attr]") {
        if !macro_name.is_empty() {
            attribute_macros.insert(macro_name.to_owned(), attributes.to_vec());
        }
        return None;
    }

    let attributes = expand_attribute_macros(attributes, attribute_macros);
    if !attributes.iter().any(|attribute| attribute == "filter=lfs") {
        return None;
    }

    Some(GitLfsTrackedPattern {
        pattern: pattern.clone(),
        attributes,
        source: source.to_path_buf(),
    })
}

fn expand_attribute_macros(
    attributes: &[String],
    attribute_macros: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    let mut expanded = Vec::new();

    for attribute in attributes {
        expand_attribute_macro(
            attribute,
            attribute_macros,
            &mut BTreeSet::new(),
            &mut expanded,
        );
    }

    expanded
}

fn expand_attribute_macro(
    attribute: &str,
    attribute_macros: &BTreeMap<String, Vec<String>>,
    expanding: &mut BTreeSet<String>,
    expanded: &mut Vec<String>,
) {
    expanded.push(attribute.to_owned());

    let Some(macro_attributes) = attribute_macros.get(attribute) else {
        return;
    };
    if !expanding.insert(attribute.to_owned()) {
        return;
    }

    for macro_attribute in macro_attributes {
        expand_attribute_macro(macro_attribute, attribute_macros, expanding, expanded);
    }

    expanding.remove(attribute);
}

fn split_gitattributes_line(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut in_quotes = false;
    let mut escaped = false;

    for ch in line.chars() {
        if escaped {
            token.push(ch);
            escaped = false;
            continue;
        }

        match ch {
            '\\' => escaped = true,
            '"' => in_quotes = !in_quotes,
            ch if ch.is_whitespace() && !in_quotes => {
                if !token.is_empty() {
                    tokens.push(std::mem::take(&mut token));
                }
            }
            _ => token.push(ch),
        }
    }

    if escaped {
        token.push('\\');
    }
    if !token.is_empty() {
        tokens.push(token);
    }

    tokens
}


#[cfg(test)]
mod attributes_tests {
    use super::test_support::*;

    #[test]
    fn parses_lfs_patterns_from_gitattributes_lines() {
        let patterns = parse_lfs_patterns_from_attributes(
            "# ignored\n\"assets/big file.bin\" filter=lfs diff=lfs -text\n*.txt text\n*.zip -text filter=lfs\n",
            Path::new(".gitattributes").to_path_buf(),
        );

        assert_eq!(patterns.len(), 2);
        assert_eq!(patterns[0].pattern, "assets/big file.bin");
        assert_eq!(
            patterns[0].attributes,
            vec!["filter=lfs", "diff=lfs", "-text"]
        );
        assert_eq!(patterns[1].pattern, "*.zip");
    }

    #[test]
    fn parses_lfs_patterns_declared_with_attribute_macros() {
        let patterns = parse_lfs_patterns_from_attributes(
            "[attr]lfs filter=lfs diff=lfs merge=lfs -text\n*.bin lfs\n",
            Path::new(".gitattributes").to_path_buf(),
        );

        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].pattern, "*.bin");
        assert_eq!(
            patterns[0].attributes,
            vec!["lfs", "filter=lfs", "diff=lfs", "merge=lfs", "-text"]
        );
    }

    #[test]
    fn parses_lfs_patterns_declared_with_nested_attribute_macros() {
        let patterns = parse_lfs_patterns_from_attributes(
            "[attr]lfs filter=lfs diff=lfs merge=lfs -text\n[attr]lfs2 lfs\n*.bin lfs2\n",
            Path::new(".gitattributes").to_path_buf(),
        );

        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].pattern, "*.bin");
        assert_eq!(
            patterns[0].attributes,
            vec![
                "lfs2",
                "lfs",
                "filter=lfs",
                "diff=lfs",
                "merge=lfs",
                "-text"
            ]
        );
    }

    #[test]
    fn splits_quoted_and_escaped_gitattributes_tokens() {
        assert_eq!(
            split_gitattributes_line(r#""assets/big file.bin" filter=lfs -text"#),
            vec!["assets/big file.bin", "filter=lfs", "-text"]
        );
        assert_eq!(
            split_gitattributes_line(r#"assets/big\ file.bin filter=lfs"#),
            vec!["assets/big file.bin", "filter=lfs"]
        );
    }

}
