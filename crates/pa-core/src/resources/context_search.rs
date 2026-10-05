//! The `claude` context search policy beyond the ancestor walk: rule
//! directories (`.claude/rules`, `.agents/rules`) in every ancestor of cwd,
//! and on demand the subtree AGENTS.md/CLAUDE.md files and path-scoped rules
//! covering files a tool call opened or edited below cwd.
//!
//! Rules without a `paths`/`globs` frontmatter key load at startup; scoped
//! rules match their globs (`!` excludes) against the file's path relative
//! to the directory holding the rules directory. On-demand context rides the
//! tool result as one marked text block per file, and a file already present
//! in the live context is not attached again (compaction drops it, so it
//! comes back on the next touch).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use pa_agent::agent_loop::AfterToolCallFn;
use pa_agent::types::{AgentMessage, Message, ToolResultContent};
use serde_json::Value;

use super::{load_context_file_from_dir, ContextFile};

const RULE_DIRS: [&str; 2] = [".claude/rules", ".agents/rules"];
const BLOCK_MARKER: &str = "[project context: ";

#[derive(Debug)]
struct ScopedRule {
    root: PathBuf,
    file: ContextFile,
    include: GlobSet,
    exclude: GlobSet,
}

impl ScopedRule {
    fn matches(&self, file: &Path) -> bool {
        file.strip_prefix(&self.root).is_ok_and(|relative| {
            self.include.is_match(relative) && !self.exclude.is_match(relative)
        })
    }
}

/// On-demand context discovery for one session.
#[derive(Debug)]
pub struct ContextSearch {
    cwd: PathBuf,
    scoped_rules: Vec<ScopedRule>,
}

/// Discover the rule files of every ancestor of `cwd`: the unscoped rules
/// (root-most first) join the startup context files, the scoped ones stay
/// in the returned [`ContextSearch`].
pub(crate) fn discover(cwd: &Path) -> (Vec<ContextFile>, ContextSearch) {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut seen = HashSet::new();
    let mut unscoped = Vec::new();
    let mut scoped_rules = Vec::new();
    let mut roots: Vec<&Path> = cwd.ancestors().collect();
    roots.reverse();
    for root in roots {
        for rule_dir in RULE_DIRS {
            let mut files = Vec::new();
            collect_markdown(&root.join(rule_dir), &mut seen, &mut files);
            for path in files {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let (frontmatter, body) = crate::skills::frontmatter::parse_frontmatter(&text);
                let file = ContextFile {
                    path,
                    content: body,
                };
                let patterns = frontmatter_patterns(&frontmatter);
                if patterns.is_empty() {
                    unscoped.push(file);
                    continue;
                }
                let (include, exclude) = glob_sets(&patterns);
                scoped_rules.push(ScopedRule {
                    root: root.to_path_buf(),
                    file,
                    include,
                    exclude,
                });
            }
        }
    }
    (unscoped, ContextSearch { cwd, scoped_rules })
}

/// `*.md` files below `dir`, sorted per directory; `seen` holds canonical
/// paths so a symlinked rules directory (`.claude/rules -> ../.agents/rules`)
/// loads once and a symlink cycle ends.
fn collect_markdown(dir: &Path, seen: &mut HashSet<PathBuf>, out: &mut Vec<PathBuf>) {
    let Ok(canonical) = dir.canonicalize() else {
        return;
    };
    if !seen.insert(canonical) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| Some(entry.ok()?.path()))
        .collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_markdown(&path, seen, out);
        } else if path.extension().is_some_and(|ext| ext == "md") {
            if let Ok(canonical) = path.canonicalize() {
                if seen.insert(canonical.clone()) {
                    out.push(canonical);
                }
            }
        }
    }
}

/// `paths` (Claude Code) or `globs` (Cursor): one pattern or a list.
fn frontmatter_patterns(frontmatter: &Value) -> Vec<String> {
    let value = frontmatter
        .get("paths")
        .or_else(|| frontmatter.get("globs"));
    match value {
        Some(Value::String(pattern)) => vec![pattern.clone()],
        Some(Value::Array(patterns)) => patterns
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn glob_sets(patterns: &[String]) -> (GlobSet, GlobSet) {
    let mut include = GlobSetBuilder::new();
    let mut exclude = GlobSetBuilder::new();
    for pattern in patterns {
        let (builder, pattern) = match pattern.strip_prefix('!') {
            Some(negated) => (&mut exclude, negated),
            None => (&mut include, pattern.as_str()),
        };
        match GlobBuilder::new(pattern).literal_separator(true).build() {
            Ok(glob) => {
                builder.add(glob);
            }
            Err(error) => tracing::warn!(%error, pattern, "invalid rule glob"),
        }
    }
    let build = |builder: GlobSetBuilder| builder.build().unwrap_or_else(|_| GlobSet::empty());
    (build(include), build(exclude))
}

/// Files a tool call touched: the `edit` target, or the files an `ipython`
/// cell opened (the kernel reports them in `openedPaths`).
fn touched_files(tool_name: &str, args: &Value, details: &Value) -> Vec<PathBuf> {
    match tool_name {
        "edit" => args
            .get("path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .into_iter()
            .collect(),
        pa_types::ai::PYTHON_TOOL_NAME => details
            .get("openedPaths")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(PathBuf::from)
            .collect(),
        _ => Vec::new(),
    }
}

/// Paths of the on-demand context blocks already in `messages`.
fn attached_paths(messages: &[AgentMessage]) -> HashSet<String> {
    messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::Standard(Message::ToolResult(result)) => Some(&result.content),
            _ => None,
        })
        .flatten()
        .filter_map(|block| match block {
            ToolResultContent::Text(text) => text.text.strip_prefix(BLOCK_MARKER),
            ToolResultContent::Image(_) => None,
        })
        .filter_map(|rest| rest.lines().next()?.strip_suffix(']'))
        .map(str::to_string)
        .collect()
}

impl ContextSearch {
    /// The context blocks covering `files` that `messages` does not hold yet.
    fn blocks_for(&self, files: &[PathBuf], messages: &[AgentMessage]) -> Vec<ToolResultContent> {
        let mut attached = attached_paths(messages);
        let mut blocks = Vec::new();
        let mut attach = |file: &ContextFile| {
            let path = file.path.display().to_string();
            if attached.insert(path.clone()) {
                blocks.push(ToolResultContent::text(format!(
                    "{BLOCK_MARKER}{path}]\n\n{}",
                    file.content
                )));
            }
        };
        for file in files {
            let Ok(file) = self.cwd.join(file).canonicalize() else {
                continue;
            };
            let Ok(relative) = file.strip_prefix(&self.cwd) else {
                continue;
            };
            let mut dir = self.cwd.clone();
            let parents = relative.parent().into_iter().flat_map(Path::components);
            for component in parents {
                dir.push(component);
                if let Some(context_file) = load_context_file_from_dir(&dir) {
                    attach(&context_file);
                }
            }
            for rule in self.scoped_rules.iter().filter(|rule| rule.matches(&file)) {
                attach(&rule.file);
            }
        }
        blocks
    }

    /// Wrap the loop's `after_tool_call` hook: `inner` runs first, then the
    /// context covering the touched files is appended to the result.
    #[must_use]
    pub fn wrap_after_tool_call(
        self: &Arc<Self>,
        inner: Option<AfterToolCallFn>,
    ) -> AfterToolCallFn {
        let search = Arc::clone(self);
        Arc::new(move |context, signal| {
            let search = Arc::clone(&search);
            let inner = inner.clone();
            Box::pin(async move {
                let overrides = match &inner {
                    Some(inner) => inner(context.clone(), signal).await?,
                    None => None,
                };
                let files = touched_files(
                    &context.tool_call.name,
                    &context.args,
                    &context.result.details,
                );
                let blocks = search.blocks_for(&files, &context.context.messages);
                if blocks.is_empty() {
                    return Ok(overrides);
                }
                let mut result = overrides.unwrap_or_default();
                let mut content = result
                    .content
                    .take()
                    .unwrap_or_else(|| context.result.content.clone());
                content.extend(blocks);
                result.content = Some(content);
                Ok(Some(result))
            })
        })
    }
}

#[cfg(test)]
#[path = "context_search_tests.rs"]
mod tests;
