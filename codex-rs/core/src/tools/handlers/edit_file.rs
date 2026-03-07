use std::collections::HashSet;
use std::io::ErrorKind;
use std::ops::Range;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;

use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::file_change::execute_verified_action;
use crate::tools::handlers::file_change::make_write_action;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;

pub struct EditFileHandler;

#[derive(Deserialize)]
struct EditFileArgs {
    file_path: String,
    edits: Vec<EditOperation>,
}

#[derive(Deserialize)]
struct EditOperation {
    old_text: String,
    new_text: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl ToolHandler for EditFileHandler {
    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn is_mutating(&self, _invocation: &ToolInvocation) -> bool {
        true
    }

    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<crate::tools::context::ToolOutput, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            tracker,
            call_id,
            tool_name,
            payload,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "edit_file handler received unsupported payload".to_string(),
                ));
            }
        };

        let args: EditFileArgs = parse_arguments(&arguments)?;
        if args.edits.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "edits must contain at least one replacement".to_string(),
            ));
        }

        let file_path = PathBuf::from(&args.file_path);
        if !file_path.is_absolute() {
            return Err(FunctionCallError::RespondToModel(
                "file_path must be an absolute path".to_string(),
            ));
        }

        let metadata = tokio::fs::metadata(&file_path).await.map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                FunctionCallError::RespondToModel("file_path does not exist".to_string())
            } else {
                FunctionCallError::RespondToModel(format!("failed to inspect file: {err}"))
            }
        })?;
        if metadata.is_dir() {
            return Err(FunctionCallError::RespondToModel(
                "file_path must point to a file".to_string(),
            ));
        }

        let old_content = tokio::fs::read_to_string(&file_path).await.map_err(|err| {
            FunctionCallError::RespondToModel(format!("failed to read file: {err}"))
        })?;
        let new_content = apply_edits(old_content.clone(), &args.edits)?;
        if new_content == old_content {
            return Err(FunctionCallError::RespondToModel(
                "edit_file would not change the file".to_string(),
            ));
        }

        let action = make_write_action(
            turn.cwd.clone(),
            file_path,
            Some(old_content.as_str()),
            new_content,
        )?;

        execute_verified_action(
            session,
            turn,
            Some(&tracker),
            &call_id,
            tool_name.as_str(),
            action,
            None,
        )
        .await
    }
}

fn apply_edits(mut content: String, edits: &[EditOperation]) -> Result<String, FunctionCallError> {
    for (index, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(FunctionCallError::RespondToModel(format!(
                "edit {} has empty old_text; use write_file to create or rewrite files",
                index + 1
            )));
        }
        if edit.old_text == edit.new_text {
            return Err(FunctionCallError::RespondToModel(format!(
                "edit {} would not change the file",
                index + 1
            )));
        }

        let matches = find_candidate_ranges(&content, &edit.old_text);
        match matches.len() {
            0 => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "edit {} old_text was not found in the file",
                    index + 1
                )));
            }
            1 => {
                let range = matches
                    .into_iter()
                    .next()
                    .expect("single match should exist");
                content.replace_range(range, &edit.new_text);
            }
            _ if edit.replace_all => {
                for range in matches.into_iter().rev() {
                    content.replace_range(range, &edit.new_text);
                }
            }
            _ => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "edit {} old_text matched multiple locations; provide more context or set replace_all",
                    index + 1
                )));
            }
        }
    }

    Ok(content)
}

fn find_candidate_ranges(content: &str, old_text: &str) -> Vec<Range<usize>> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for matcher in [
        exact_match_ranges as fn(&str, &str) -> Vec<Range<usize>>,
        line_trimmed_match_ranges,
        whitespace_normalized_match_ranges,
        indentation_flexible_match_ranges,
        context_anchor_match_ranges,
    ] {
        for range in matcher(content, old_text) {
            if seen.insert((range.start, range.end)) {
                candidates.push(range);
            }
        }
    }
    candidates.sort_by_key(|range| range.start);
    candidates
}

fn exact_match_ranges(content: &str, old_text: &str) -> Vec<Range<usize>> {
    content
        .match_indices(old_text)
        .map(|(start, matched)| start..start + matched.len())
        .collect()
}

fn line_trimmed_match_ranges(content: &str, old_text: &str) -> Vec<Range<usize>> {
    let content_lines = split_lines_preserve_offsets(content);
    let include_trailing_newline = old_text.ends_with('\n');
    let mut search_lines: Vec<&str> = old_text.split('\n').collect();
    if search_lines.last() == Some(&"") {
        search_lines.pop();
    }
    if search_lines.is_empty() || search_lines.len() > content_lines.len() {
        return Vec::new();
    }

    let mut matches = Vec::new();
    for window_start in 0..=content_lines.len() - search_lines.len() {
        let mut ok = true;
        for (offset, search_line) in search_lines.iter().enumerate() {
            if content_lines[window_start + offset].text.trim() != search_line.trim() {
                ok = false;
                break;
            }
        }
        if ok {
            matches.push(line_window_range(
                &content_lines,
                window_start,
                search_lines.len(),
                content,
                include_trailing_newline,
            ));
        }
    }
    matches
}

fn whitespace_normalized_match_ranges(content: &str, old_text: &str) -> Vec<Range<usize>> {
    let normalized_find = normalize_whitespace(old_text);
    if normalized_find.is_empty() {
        return Vec::new();
    }

    let content_lines = split_lines_preserve_offsets(content);
    let include_trailing_newline = old_text.ends_with('\n');
    let mut matches = Vec::new();

    for line in &content_lines {
        if normalize_whitespace(line.text) == normalized_find {
            matches.push(line.start..line.end);
        }
    }

    let mut search_lines: Vec<&str> = old_text.split('\n').collect();
    if search_lines.last() == Some(&"") {
        search_lines.pop();
    }
    if search_lines.len() > 1 && search_lines.len() <= content_lines.len() {
        for window_start in 0..=content_lines.len() - search_lines.len() {
            let window = &content_lines[window_start..window_start + search_lines.len()];
            let block = window
                .iter()
                .map(|line| line.text)
                .collect::<Vec<_>>()
                .join("\n");
            if normalize_whitespace(&block) == normalized_find {
                matches.push(line_window_range(
                    &content_lines,
                    window_start,
                    search_lines.len(),
                    content,
                    include_trailing_newline,
                ));
            }
        }
    }

    matches
}

fn indentation_flexible_match_ranges(content: &str, old_text: &str) -> Vec<Range<usize>> {
    let content_lines = split_lines_preserve_offsets(content);
    let include_trailing_newline = old_text.ends_with('\n');
    let mut search_lines: Vec<&str> = old_text.split('\n').collect();
    if search_lines.last() == Some(&"") {
        search_lines.pop();
    }
    if search_lines.is_empty() || search_lines.len() > content_lines.len() {
        return Vec::new();
    }

    let normalized_find = remove_shared_indentation(&search_lines.join("\n"));
    let mut matches = Vec::new();
    for window_start in 0..=content_lines.len() - search_lines.len() {
        let window = &content_lines[window_start..window_start + search_lines.len()];
        let block = window
            .iter()
            .map(|line| line.text)
            .collect::<Vec<_>>()
            .join("\n");
        if remove_shared_indentation(&block) == normalized_find {
            matches.push(line_window_range(
                &content_lines,
                window_start,
                search_lines.len(),
                content,
                include_trailing_newline,
            ));
        }
    }
    matches
}

fn context_anchor_match_ranges(content: &str, old_text: &str) -> Vec<Range<usize>> {
    let content_lines = split_lines_preserve_offsets(content);
    let include_trailing_newline = old_text.ends_with('\n');
    let mut search_lines: Vec<&str> = old_text.split('\n').collect();
    if search_lines.last() == Some(&"") {
        search_lines.pop();
    }
    if search_lines.len() < 3 || search_lines.len() > content_lines.len() {
        return Vec::new();
    }

    let first_line = search_lines[0].trim();
    let last_line = search_lines[search_lines.len() - 1].trim();
    let mut candidates = Vec::new();

    for start_index in 0..content_lines.len() {
        if content_lines[start_index].text.trim() != first_line {
            continue;
        }
        for end_index in start_index + 2..content_lines.len() {
            if content_lines[end_index].text.trim() != last_line {
                continue;
            }
            candidates.push((start_index, end_index));
            break;
        }
    }

    if candidates.is_empty() {
        return Vec::new();
    }

    let mut best_similarity = -1.0f64;
    let mut best_range: Option<Range<usize>> = None;
    for (start_index, end_index) in candidates {
        let middle_len = (search_lines.len().saturating_sub(2))
            .min(end_index.saturating_sub(start_index).saturating_sub(1));
        let similarity = if middle_len == 0 {
            1.0
        } else {
            let mut total = 0.0;
            for offset in 0..middle_len {
                let content_line = content_lines[start_index + offset + 1].text.trim();
                let search_line = search_lines[offset + 1].trim();
                total += line_similarity(content_line, search_line);
            }
            total / middle_len as f64
        };
        if similarity > best_similarity {
            best_similarity = similarity;
            best_range = Some(line_window_range(
                &content_lines,
                start_index,
                end_index - start_index + 1,
                content,
                include_trailing_newline,
            ));
        }
    }

    if best_similarity >= 0.5 {
        best_range.into_iter().collect()
    } else {
        Vec::new()
    }
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn line_window_range(
    content_lines: &[LineSlice<'_>],
    start_index: usize,
    line_count: usize,
    content: &str,
    include_trailing_newline: bool,
) -> Range<usize> {
    let start = content_lines[start_index].start;
    let last = &content_lines[start_index + line_count - 1];
    let mut end = last.end;
    if include_trailing_newline && content.as_bytes().get(end) == Some(&b'\n') {
        end += 1;
    }
    start..end
}

fn remove_shared_indentation(value: &str) -> String {
    let lines: Vec<&str> = value.split('\n').collect();
    let non_empty: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if non_empty.is_empty() {
        return value.to_string();
    }

    let min_indent = non_empty
        .iter()
        .map(|line| line.chars().take_while(|ch| ch.is_whitespace()).count())
        .min()
        .unwrap_or(0);

    lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                (*line).to_string()
            } else {
                line.chars().skip(min_indent).collect()
            }
        })
        .collect::<Vec<String>>()
        .join("\n")
}

fn line_similarity(left: &str, right: &str) -> f64 {
    let max_len = left.len().max(right.len());
    if max_len == 0 {
        return 1.0;
    }
    1.0 - levenshtein(left, right) as f64 / max_len as f64
}

fn levenshtein(left: &str, right: &str) -> usize {
    if left.is_empty() || right.is_empty() {
        return left.len().max(right.len());
    }

    let left_chars: Vec<char> = left.chars().collect();
    let right_chars: Vec<char> = right.chars().collect();
    let mut previous_row: Vec<usize> = (0..=right_chars.len()).collect();
    let mut current_row = vec![0; right_chars.len() + 1];

    for (left_index, left_char) in left_chars.iter().enumerate() {
        current_row[0] = left_index + 1;
        for (right_index, right_char) in right_chars.iter().enumerate() {
            let substitution_cost = if left_char == right_char { 0 } else { 1 };
            current_row[right_index + 1] = (previous_row[right_index + 1] + 1)
                .min(current_row[right_index] + 1)
                .min(previous_row[right_index] + substitution_cost);
        }
        std::mem::swap(&mut previous_row, &mut current_row);
    }

    previous_row[right_chars.len()]
}

fn split_lines_preserve_offsets(content: &str) -> Vec<LineSlice<'_>> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for segment in content.split_inclusive('\n') {
        let mut text = segment;
        if let Some(stripped) = text.strip_suffix('\n') {
            text = stripped;
        }
        lines.push(LineSlice {
            text,
            start,
            end: start + text.len(),
        });
        start += segment.len();
    }

    if !content.ends_with('\n')
        && let Some(tail) = content.get(start..)
            && !tail.is_empty() {
                lines.push(LineSlice {
                    text: tail,
                    start,
                    end: content.len(),
                });
            }

    lines
}

struct LineSlice<'a> {
    text: &'a str,
    start: usize,
    end: usize,
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn apply_edits_replaces_exact_match() {
        let edits = vec![EditOperation {
            old_text: "beta".to_string(),
            new_text: "BETA".to_string(),
            replace_all: false,
        }];

        let updated = apply_edits("alpha\nbeta\ngamma\n".to_string(), &edits)
            .expect("apply exact replacement");
        assert_eq!(updated, "alpha\nBETA\ngamma\n");
    }

    #[test]
    fn apply_edits_requires_replace_all_for_ambiguous_match() {
        let edits = vec![EditOperation {
            old_text: "dup".to_string(),
            new_text: "value".to_string(),
            replace_all: false,
        }];

        let err = apply_edits("dup\ndup\n".to_string(), &edits).expect_err("ambiguous edit");
        assert_eq!(
            err,
            FunctionCallError::RespondToModel(
                "edit 1 old_text matched multiple locations; provide more context or set replace_all"
                    .to_string()
            )
        );
    }

    #[test]
    fn apply_edits_matches_trimmed_lines() {
        let edits = vec![EditOperation {
            old_text: "line2   ".to_string(),
            new_text: "changed".to_string(),
            replace_all: false,
        }];

        let updated = apply_edits("  line1\n line2 \nline3\n".to_string(), &edits)
            .expect("trimmed line match");
        assert_eq!(updated, "  line1\nchanged\nline3\n");
    }

    #[test]
    fn apply_edits_matches_indentation_flexible_block() {
        let edits = vec![EditOperation {
            old_text: "if ready:\n    run()\n".to_string(),
            new_text: "if ready:\n    launch()\n".to_string(),
            replace_all: false,
        }];

        let updated = apply_edits(
            "fn main():\n        if ready:\n            run()\n".to_string(),
            &edits,
        )
        .expect("indentation flexible match");
        assert_eq!(updated, "fn main():\nif ready:\n    launch()\n");
    }

    #[test]
    fn apply_edits_replace_all_updates_multiple_ranges() {
        let edits = vec![EditOperation {
            old_text: "value".to_string(),
            new_text: "item".to_string(),
            replace_all: true,
        }];

        let updated =
            apply_edits("value\nother\nvalue\n".to_string(), &edits).expect("replace all");
        assert_eq!(updated, "item\nother\nitem\n");
    }
}
