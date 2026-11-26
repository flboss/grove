use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use grove_query::ParseOutcome;
use rustyline::error::ReadlineError;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{ReplOutcome, ReplSession};
use crate::args::ConnectionSpec;
use crate::diagnostics;
use crate::error::{CliError, CliResult};
use crate::render;

impl ReplSession<'_> {
    pub(super) fn run(&mut self, editor: &mut rustyline::DefaultEditor) -> CliResult<ReplOutcome> {
        let mut buffer = String::new();

        loop {
            let (prompt, continuation) = self.prompts();
            let active_prompt = if buffer.is_empty() {
                prompt.as_str()
            } else {
                continuation.as_str()
            };
            match editor.readline(active_prompt) {
                Ok(line) => {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        let _ = editor.add_history_entry(line.as_str());
                    }

                    if buffer.is_empty() && trimmed.starts_with('.') {
                        if let Some(outcome) = self.meta_command(trimmed)? {
                            return Ok(outcome);
                        }
                        continue;
                    }

                    if !buffer.is_empty() {
                        buffer.push('\n');
                    }
                    buffer.push_str(&line);

                    if buffer.trim().is_empty() {
                        buffer.clear();
                        continue;
                    }

                    let source_name = format!("<repl:{}>", *self.turn);
                    match grove_query::parse_query_repl(&buffer) {
                        ParseOutcome::Complete { file, diagnostics } => {
                            *self.turn += 1;
                            diagnostics::print(&buffer, &source_name, &diagnostics);
                            let _ = self.execute_file(file, &buffer, &source_name)?;
                            buffer.clear();
                        }
                        ParseOutcome::Incomplete { .. } => {}
                        ParseOutcome::Invalid { diagnostics } => {
                            *self.turn += 1;
                            diagnostics::print(&buffer, &source_name, &diagnostics);
                            buffer.clear();
                        }
                    }
                }
                Err(ReadlineError::Interrupted) => {
                    buffer.clear();
                }
                Err(ReadlineError::Eof) => {
                    if buffer.is_empty() {
                        return Ok(ReplOutcome::Quit);
                    }
                    buffer.clear();
                }
                Err(error) => {
                    return Err(CliError::runtime(format!("REPL input error: {error}")));
                }
            }
        }
    }

    pub(super) fn prompts(&self) -> (String, String) {
        let colored = self.settings.output.repl_color_enabled();
        let schema_name = truncated_file_name(&self.schema_path, 24);
        let mut prompt = String::new();
        let mut prompt_width = 0;

        const PROMPT_GROVE: &str = "grove";
        prompt.push_str(&render::paint(PROMPT_GROVE, Some("1;36"), colored));
        prompt.push(' ');
        prompt.push_str(&render::paint(schema_name.as_ref(), Some("1;32"), colored));
        prompt.push(' ');
        prompt_width += PROMPT_GROVE.width() + 1 + schema_name.width() + 1;

        match self.spec {
            ConnectionSpec::Sqlite { file, .. } => {
                const PROMPT_SQLITE: &str = "SQLite:";
                let file_name = truncated_file_name(file, 24);
                prompt.push_str(&render::paint(PROMPT_SQLITE, Some("34"), colored));
                prompt.push(' ');
                prompt.push_str(&render::paint(file_name.as_ref(), Some("1;34"), colored));
                prompt.push(' ');
                prompt_width += PROMPT_SQLITE.width() + 1 + file_name.width() + 1;
            }
            ConnectionSpec::Memory => {
                const PROMPT_INMEMORY: &str = "in-memory";
                prompt.push_str(&render::paint(PROMPT_INMEMORY, Some("1;35"), colored));
                prompt.push(' ');
                prompt_width += PROMPT_INMEMORY.width() + 1;
            }
        }

        const PROMPT_MARKER: &str = "> ";
        let prompt_marker = render::paint(PROMPT_MARKER, Some("36"), colored);
        prompt.push_str(&prompt_marker);

        const CONTINUATION_PREFIX: &str = "...";
        let continuation_prefix = render::paint(CONTINUATION_PREFIX, Some("36"), colored);
        let mut continuation = String::with_capacity(
            prompt_width - CONTINUATION_PREFIX.width()
                + continuation_prefix.len()
                + prompt_marker.len(),
        );
        continuation.extend(std::iter::repeat_n(
            ' ',
            prompt_width - CONTINUATION_PREFIX.width(),
        ));
        continuation.push_str(&continuation_prefix);
        continuation.push_str(&prompt_marker);
        (prompt, continuation)
    }
}

fn truncated_file_name(path: &Path, max_width: usize) -> Cow<'_, str> {
    if max_width == 1 {
        return Cow::Borrowed("…");
    }
    let file_name = path
        .file_name()
        .map(OsStr::to_string_lossy)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| Cow::Owned(path.display().to_string()));

    if max_width == 0 || UnicodeWidthStr::width(file_name.as_ref()) <= max_width {
        return file_name;
    }
    let file_name = file_name.into_owned();
    let mut out = String::new();
    let mut width = 0;
    for ch in file_name.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + char_width > max_width - 1 {
            break;
        }
        out.push(ch);
        width += char_width;
    }
    out.push('…');
    Cow::Owned(out)
}

pub(super) fn history_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
        })?;
    Some(base.join("grove").join("history"))
}
