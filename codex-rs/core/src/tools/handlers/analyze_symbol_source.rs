use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;

use serde::Deserialize;
use serde::Serialize;
use tempfile::NamedTempFile;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;

#[cfg(feature = "clang-graph")]
use super::clang_graph::CallGraph;
#[cfg(feature = "clang-graph")]
use super::clang_graph::CompileDbLoader;
#[cfg(feature = "clang-graph")]
use super::clang_graph::TuParser;

#[cfg(feature = "clang-graph")]
use super::clang_graph::BfsConfig;
#[cfg(feature = "clang-graph")]
use super::clang_graph::BfsPriority;
#[cfg(feature = "clang-graph")]
use super::clang_graph::BfsResult;
#[cfg(feature = "clang-graph")]
use super::clang_graph::ClangEngine;
#[cfg(feature = "clang-graph")]
use super::clang_graph::bfs_call_graph;

use super::dir_stats::estimate_scope_file_count;
use super::dir_stats::load_dir_stats;
use super::search_rg::make_scope_tempfile;
use super::search_rg::run_rg_lines_direct;
use super::search_rg::run_rg_lines_from_manifest;
use super::workspace_index::IndexStatus;
use super::workspace_index::WorkspaceIndexConfig;
use super::workspace_index::ensure_index;

pub struct AnalyzeSymbolSourceHandler;

fn default_max_callers() -> usize {
    15
}

fn default_max_callees() -> usize {
    20
}

fn default_context_lines() -> usize {
    50
}

fn default_true() -> bool {
    true
}

fn default_max_depth() -> u32 {
    0
}

fn default_max_nodes() -> usize {
    200
}

fn default_max_edges() -> usize {
    500
}

/// Engine selection for call graph analysis.
#[derive(Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum EngineChoice {
    /// Auto-detect: use clang if available, otherwise heuristic.
    #[default]
    Auto,
    /// Force libclang-based analysis (fails if unavailable).
    Clang,
    /// Force ripgrep heuristic analysis.
    Heuristic,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnalyzeSymbolSourceArgs {
    symbol: String,
    #[serde(default)]
    scope_path: Option<String>,
    #[serde(default = "default_max_callers")]
    max_callers: usize,
    #[serde(default = "default_max_callees")]
    max_callees: usize,
    #[serde(default = "default_context_lines")]
    context_lines: usize,
    #[serde(default = "default_true")]
    include_source: bool,
    #[serde(default = "default_true")]
    include_tests: bool,
    #[serde(default)]
    verbose: bool,
    /// Depth of the call graph traversal (0 = legacy, 1-2 = BFS graph).
    #[serde(default = "default_max_depth")]
    max_depth: u32,
    /// Engine to use: "auto", "clang", or "heuristic".
    #[serde(default)]
    #[cfg_attr(not(feature = "clang-graph"), allow(dead_code))]
    engine: EngineChoice,
    /// Maximum nodes in the graph output.
    #[serde(default = "default_max_nodes")]
    #[cfg_attr(not(feature = "clang-graph"), allow(dead_code))]
    max_nodes: usize,
    /// Maximum edges in the graph output.
    #[serde(default = "default_max_edges")]
    #[cfg_attr(not(feature = "clang-graph"), allow(dead_code))]
    max_edges: usize,
}

#[derive(Serialize)]
pub struct DefinitionResult {
    pub file: String,
    pub line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Serialize)]
pub struct CallerResult {
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct CalleeResult {
    pub callee: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(rename = "callType", skip_serializing_if = "Option::is_none")]
    pub call_type: Option<String>,
}

#[derive(Serialize)]
struct AnalysisOutput {
    success: bool,
    symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    definition: Option<DefinitionResult>,
    callers: Vec<CallerResult>,
    #[serde(rename = "testCallers", skip_serializing_if = "Option::is_none")]
    test_callers: Option<Vec<CallerResult>>,
    callees: Vec<CalleeResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timing: Option<TimingInfo>,
    /// When true, the results were validated/enhanced by libclang AST analysis.
    #[serde(rename = "clangEnhanced", skip_serializing_if = "is_false")]
    clang_enhanced: bool,
    /// Optional depth-limited BFS call graph (populated when maxDepth > 0).
    #[serde(skip_serializing_if = "Option::is_none")]
    graph: Option<GraphOutput>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !b
}

/// Serializable BFS graph output, wrapping the types from `bfs_traversal`.
///
/// We use our own thin struct so the output is stable even if the internal
/// BFS types evolve.  The `From<BfsResult>` conversion is feature-gated
/// below.
#[derive(Serialize)]
struct GraphOutput {
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    #[serde(rename = "maxDepthReached")]
    max_depth_reached: u32,
    truncated: bool,
    engine: String,
}

#[derive(Serialize)]
struct GraphNode {
    id: String,
    name: String,
    file: String,
    line: u32,
    depth: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    usr: Option<String>,
    #[serde(rename = "isDefinition")]
    is_definition: bool,
}

#[derive(Serialize)]
struct GraphEdge {
    from: String,
    to: String,
    #[serde(rename = "callType")]
    call_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<u32>,
}

#[cfg(feature = "clang-graph")]
impl From<BfsResult> for GraphOutput {
    fn from(r: BfsResult) -> Self {
        Self {
            nodes: r
                .nodes
                .into_iter()
                .map(|n| GraphNode {
                    id: n.id,
                    name: n.name,
                    file: n.file,
                    line: n.line,
                    depth: n.depth,
                    usr: n.usr,
                    is_definition: n.is_definition,
                })
                .collect(),
            edges: r
                .edges
                .into_iter()
                .map(|e| GraphEdge {
                    from: e.from,
                    to: e.to,
                    call_type: e.call_type,
                    file: e.file,
                    line: e.line,
                })
                .collect(),
            max_depth_reached: r.max_depth_reached,
            truncated: r.truncated,
            engine: r.engine,
        }
    }
}

#[derive(Serialize)]
struct TimingInfo {
    definition_ms: u64,
    references_ms: u64,
    total_ms: u64,
}

impl ToolHandler for AnalyzeSymbolSourceHandler {
    type Output = FunctionToolOutput;

    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<Self::Output, FunctionCallError> {
        let ToolInvocation { payload, turn, .. } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "analyze_symbol_source handler received unsupported payload".to_string(),
                ));
            }
        };

        let args: AnalyzeSymbolSourceArgs = parse_arguments(&arguments)?;

        let symbol = args.symbol.trim().to_string();
        if symbol.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "symbol must not be empty".to_string(),
            ));
        }

        let scope_path = turn.resolve_path(args.scope_path.clone());
        let workspace_root = turn.cwd.clone();
        let config = WorkspaceIndexConfig::from_env();

        // Determine the search strategy based on the workspace index state.
        // When the manifest is available use it to avoid directory traversal / stat
        // storms on NFS.  Otherwise fall back to direct `rg` invocations.
        let search_strategy =
            resolve_search_strategy(&scope_path, &workspace_root, &config).await?;

        if let SearchStrategy::TooManyFiles { count } = search_strategy {
            return Ok(make_too_broad_output(&symbol, &scope_path, count));
        }
        if let SearchStrategy::IndexBuilding = search_strategy {
            return Ok(make_building_output(&symbol));
        }

        let total_start = Instant::now();

        // Phase 1: definition search.
        let def_start = Instant::now();
        let base_name = base_symbol_name(&symbol).to_string();
        let def_pattern = build_definition_pattern(&base_name);
        let definition_ms;
        let definition = match search_lines(
            &def_pattern,
            &search_strategy,
            &scope_path,
            /*max_results=*/ 20,
            /*max_count_per_file=*/ Some(1),
            &workspace_root,
        )
        .await
        {
            Ok(hits) => {
                definition_ms = def_start.elapsed().as_millis() as u64;
                hits.into_iter().next().map(|(file, line, content)| {
                    let signature = content.trim().to_string();
                    let source = if args.include_source {
                        read_source_snippet_sync(&file, line, args.context_lines)
                    } else {
                        None
                    };
                    DefinitionResult {
                        file,
                        line,
                        signature: if signature.is_empty() {
                            None
                        } else {
                            Some(signature)
                        },
                        source,
                    }
                })
            }
            Err(_) => {
                definition_ms = def_start.elapsed().as_millis() as u64;
                None
            }
        };

        // Phase 2: reference search for callers.
        let ref_start = Instant::now();
        let mut callers: Vec<CallerResult> = vec![];
        let mut test_callers: Vec<CallerResult> = vec![];

        if args.max_callers > 0 {
            let ref_pattern = format!(r"\b{}\b", regex_escape(&base_name));
            if let Ok(hits) = search_lines(
                &ref_pattern,
                &search_strategy,
                &scope_path,
                args.max_callers * 10,
                /*max_count_per_file=*/ None,
                &workspace_root,
            )
            .await
            {
                let def_key = definition.as_ref().map(|d| (d.file.clone(), d.line));
                let mut seen: HashSet<(String, u32)> = HashSet::new();
                for (file, line, content) in hits {
                    let key = (file.clone(), line);
                    if Some(&key) == def_key.as_ref() {
                        continue;
                    }
                    if !seen.insert(key) {
                        continue;
                    }
                    let caller = CallerResult {
                        file: file.clone(),
                        line: Some(line),
                        context: Some(content.trim().to_string()),
                    };
                    if is_test_file(&file) {
                        test_callers.push(caller);
                    } else {
                        callers.push(caller);
                    }
                }
                callers.truncate(args.max_callers);
                test_callers.truncate(args.max_callers);
            }
        }
        let references_ms = ref_start.elapsed().as_millis() as u64;

        // Phase 3: callee extraction from the definition snippet.
        #[cfg_attr(not(feature = "clang-graph"), allow(unused_mut))]
        let mut callees = definition
            .as_ref()
            .and_then(|d| d.source.as_deref())
            .map(|src| extract_callees(src, args.max_callees, &base_name))
            .unwrap_or_default();

        // Phase 4 (optional): libclang-based validation when the
        // `clang-graph` feature is enabled and compile_commands.json
        // is discoverable near the scope path.
        let clang_enhanced = {
            #[cfg(feature = "clang-graph")]
            {
                try_clang_enhance(
                    &scope_path,
                    &base_name,
                    &definition,
                    &mut callers,
                    &mut callees,
                    args.max_callers,
                    args.max_callees,
                )
            }
            #[cfg(not(feature = "clang-graph"))]
            {
                false
            }
        };

        // Phase 5 (optional): BFS call graph when max_depth > 0.
        let graph: Option<GraphOutput> = if args.max_depth > 0 {
            #[cfg(feature = "clang-graph")]
            {
                run_bfs_phase(
                    &scope_path,
                    &base_name,
                    &definition,
                    &callers,
                    &callees,
                    &args,
                )
            }
            #[cfg(not(feature = "clang-graph"))]
            {
                None
            }
        } else {
            None
        };

        let total_ms = total_start.elapsed().as_millis() as u64;

        let timing = if args.verbose {
            Some(TimingInfo {
                definition_ms,
                references_ms,
                total_ms,
            })
        } else {
            None
        };

        let test_callers_out = if args.include_tests && !test_callers.is_empty() {
            Some(test_callers)
        } else {
            None
        };

        let output = AnalysisOutput {
            success: true,
            symbol,
            definition,
            callers,
            test_callers: test_callers_out,
            callees,
            error: None,
            timing,
            clang_enhanced,
            graph,
        };

        let json = serde_json::to_string_pretty(&output).map_err(|e| {
            FunctionCallError::RespondToModel(format!("failed to serialize output: {e}"))
        })?;

        Ok(FunctionToolOutput::from_text(json, Some(true)))
    }
}

// ---------------------------------------------------------------------------
// Search strategy
// ---------------------------------------------------------------------------

/// Describes how searches will be executed for this invocation.
///
/// Resolved once per `handle()` call and then shared between the definition
/// and caller searches to avoid redundant index checks.
enum SearchStrategy {
    /// Use the pre-filtered manifest temp file (NFS-safe, fast).
    Manifest(NamedTempFile),
    /// Fall back to a direct directory scan (index not yet ready).
    Direct,
    /// The index is currently being built and the scope is the whole workspace.
    /// The caller should return a "please retry" message.
    IndexBuilding,
    /// The scope contains more files than the configured limit.
    TooManyFiles { count: usize },
}

/// Resolves the [`SearchStrategy`] to use for the given scope.
async fn resolve_search_strategy(
    scope_path: &Path,
    workspace_root: &Path,
    config: &WorkspaceIndexConfig,
) -> Result<SearchStrategy, FunctionCallError> {
    match ensure_index(workspace_root, config) {
        IndexStatus::Ready {
            ref manifest_path,
            ref dirstats_path,
        } => {
            // Scope governor: use the dirstats file (no traversal needed).
            if let Ok(stats) = load_dir_stats(dirstats_path) {
                let count = estimate_scope_file_count(&stats, scope_path);
                if count > config.max_scope_files {
                    return Ok(SearchStrategy::TooManyFiles { count });
                }
            }
            // Build the scoped temp file; fall back to direct if empty/error.
            match make_scope_tempfile(manifest_path, scope_path) {
                Some(tmp) => Ok(SearchStrategy::Manifest(tmp)),
                None => Ok(SearchStrategy::Direct),
            }
        }
        IndexStatus::Building if scope_path == workspace_root => Ok(SearchStrategy::IndexBuilding),
        IndexStatus::Building | IndexStatus::Stale | IndexStatus::Unavailable => {
            // For scoped (sub-directory) searches fall back to direct rg.
            // Apply a cheap file-count governor to avoid runaway scans.
            let count = count_files_in_scope(scope_path).await.unwrap_or(0);
            if count > config.max_scope_files {
                Ok(SearchStrategy::TooManyFiles { count })
            } else {
                Ok(SearchStrategy::Direct)
            }
        }
    }
}

/// Run a line-based rg search using the resolved [`SearchStrategy`].
async fn search_lines(
    pattern: &str,
    strategy: &SearchStrategy,
    scope_path: &Path,
    max_results: usize,
    max_count_per_file: Option<usize>,
    cwd: &Path,
) -> Result<Vec<(String, u32, String)>, String> {
    match strategy {
        SearchStrategy::Manifest(tmp) => {
            run_rg_lines_from_manifest(pattern, tmp.path(), max_results, max_count_per_file, cwd)
                .await
        }
        SearchStrategy::Direct => {
            run_rg_lines_direct(pattern, scope_path, max_results, max_count_per_file).await
        }
        // These two variants are handled before search_lines is called.
        SearchStrategy::IndexBuilding | SearchStrategy::TooManyFiles { .. } => Ok(vec![]),
    }
}

fn make_too_broad_output(symbol: &str, scope_path: &Path, count: usize) -> FunctionToolOutput {
    let output = AnalysisOutput {
        success: false,
        symbol: symbol.to_string(),
        definition: None,
        callers: vec![],
        test_callers: None,
        callees: vec![],
        error: Some(format!(
            "Scope too broad: {count} files found under `{}`. \
             Provide a narrower `scopePath` (e.g. a sub-directory) to limit the search.",
            scope_path.display()
        )),
        timing: None,
        clang_enhanced: false,
        graph: None,
    };
    let json =
        serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{\"success\":false}".to_string());
    FunctionToolOutput::from_text(json, Some(false))
}

fn make_building_output(symbol: &str) -> FunctionToolOutput {
    let output = AnalysisOutput {
        success: false,
        symbol: symbol.to_string(),
        definition: None,
        callers: vec![],
        test_callers: None,
        callees: vec![],
        error: Some(
            "Workspace index is being built in the background. \
             Please retry in a moment, or provide a narrower `scopePath` to search a sub-directory immediately."
                .to_string(),
        ),
        timing: None,
        clang_enhanced: false,
        graph: None,
    };
    let json =
        serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{\"success\":false}".to_string());
    FunctionToolOutput::from_text(json, Some(false))
}

// ---------------------------------------------------------------------------
// Public helpers (also exercised by tests)
// ---------------------------------------------------------------------------

/// Returns the base name of a (possibly qualified) symbol.
///
/// For `MyStruct::my_method` returns `my_method`.
/// For `pkg.MyStruct.Method` returns `Method`.
/// For plain `my_func` returns `my_func`.
/// When both `::` and `.` appear, the rightmost separator wins.
pub fn base_symbol_name(symbol: &str) -> &str {
    let colon_end = symbol.rfind("::").map(|i| i + 2);
    let dot_end = symbol.rfind('.').map(|i| i + 1);
    match (colon_end, dot_end) {
        (Some(c), Some(d)) => &symbol[c.max(d)..],
        (Some(c), None) => &symbol[c..],
        (None, Some(d)) => &symbol[d..],
        (None, None) => symbol,
    }
}

/// Returns `true` if the file path looks like a test file.
pub fn is_test_file(path: &str) -> bool {
    // Normalize backslashes so Windows paths are detected correctly.
    let p = path.to_lowercase().replace('\\', "/");
    p.contains("/test/")
        || p.contains("/tests/")
        || p.contains("_test.")
        || p.contains(".test.")
        || p.contains(".spec.")
        || p.ends_with("_test")
        || p.ends_with("_tests")
}

/// Build a ripgrep-compatible regex pattern that matches likely definition lines
/// for the given base symbol name.
pub fn build_definition_pattern(base_name: &str) -> String {
    let escaped = regex_escape(base_name);
    // Covers Rust (fn/struct/trait/enum/type/const/impl), Python (def/class),
    // JS/TS (function/class/const/let), Go (func), and more.
    format!(
        r"^\s*(pub(\s*\([^)]*\))?\s+|async\s+|pub\s+async\s+|export\s+)?(fn|def|class|struct|trait|interface|function|type|enum|const|let|var|func)\s+{escaped}\s*[<({{]"
    )
}

/// Extract callee (function/method call) names from a source snippet.
///
/// Skips comment lines and language keywords to reduce noise.
pub fn extract_callees(source: &str, max_callees: usize, self_name: &str) -> Vec<CalleeResult> {
    const KEYWORDS: &[&str] = &[
        "if",
        "while",
        "for",
        "loop",
        "match",
        "switch",
        "catch",
        "try",
        "return",
        "assert",
        "panic",
        "todo",
        "unimplemented",
        "unreachable",
        "println",
        "eprintln",
        "print",
        "eprint",
        "format",
        "write",
        "writeln",
        "vec",
        "box",
        "drop",
        "clone",
        "new",
    ];

    let mut results: Vec<CalleeResult> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for (idx, raw_line) in source.lines().enumerate() {
        let line = raw_line.trim();

        // Skip blank or comment-only lines.
        if line.is_empty()
            || line.starts_with("//")
            || line.starts_with('#')
            || line.starts_with("/*")
            || line.starts_with('*')
            || line.starts_with("--")
        {
            continue;
        }

        let code = strip_inline_comment(line);

        // Walk the line looking for `identifier(` patterns.
        let mut pos = 0;
        while pos < code.len() {
            let Some(rel_paren) = code[pos..].find('(') else {
                break;
            };
            let abs_paren = pos + rel_paren;
            let before = &code[..abs_paren];
            let ident = extract_identifier_before(before);
            pos = abs_paren + 1;

            if ident.is_empty() || ident.len() <= 1 {
                continue;
            }

            let call_type = if before.ends_with(&format!(".{ident}")) {
                "method"
            } else if before.ends_with(&format!("::{ident}")) {
                "static"
            } else {
                "function"
            };

            let lower = ident.to_lowercase();
            if KEYWORDS.contains(&lower.as_str()) || ident == self_name {
                continue;
            }

            if seen.insert(ident.clone()) {
                results.push(CalleeResult {
                    callee: ident,
                    line: Some((idx + 1) as u32),
                    call_type: Some(call_type.to_string()),
                });
                if results.len() >= max_callees {
                    return results;
                }
            }
        }
    }

    results
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Escape regex special characters so an identifier can be used as a literal.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if matches!(
            ch,
            '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Strip an inline `//` or ` #` comment from a code line (best-effort).
fn strip_inline_comment(line: &str) -> &str {
    if let Some(idx) = line.find("//") {
        return &line[..idx];
    }
    if let Some(idx) = line.find(" #") {
        return &line[..idx];
    }
    line
}

/// Given the text before an opening `(`, extract the trailing identifier.
fn extract_identifier_before(before: &str) -> String {
    let trimmed = before.trim_end();
    let bytes = trimmed.as_bytes();
    let mut end = bytes.len();
    while end > 0 && is_ident_char(bytes[end - 1]) {
        end -= 1;
    }
    trimmed[end..].to_string()
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Count files under a scope path using `rg --files`.
///
/// Used in the fallback path when the workspace index is not yet available,
/// to apply the `max_scope_files` governor without a manifest.
async fn count_files_in_scope(scope_path: &Path) -> Result<usize, String> {
    use std::time::Duration;
    use tokio::process::Command;
    use tokio::time::timeout;

    const TIMEOUT: Duration = Duration::from_secs(30);

    let output = timeout(
        TIMEOUT,
        Command::new("rg")
            .arg("--files")
            .arg("--no-messages")
            .arg("--")
            .arg(scope_path)
            .output(),
    )
    .await
    .map_err(|_| "rg --files timed out".to_string())?
    .map_err(|e| format!("failed to launch rg: {e}"))?;

    match output.status.code() {
        Some(0) | Some(1) => {
            let text = String::from_utf8_lossy(&output.stdout);
            Ok(text.lines().filter(|l| !l.is_empty()).count())
        }
        _ => Err(format!(
            "rg --files failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )),
    }
}

/// Read `context_lines` lines of source around `line_number` from a file.
fn read_source_snippet_sync(file: &str, line_number: u32, context_lines: usize) -> Option<String> {
    let content = std::fs::read_to_string(file).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let zero_idx = (line_number as usize).saturating_sub(1);
    // Start a couple lines before to capture doc comments / attributes.
    let start = zero_idx.saturating_sub(2);
    let end = (zero_idx + context_lines).min(lines.len());
    Some(lines[start..end].join("\n"))
}

// ---------------------------------------------------------------------------
// Optional BFS call graph (gated behind `clang-graph` feature)
// ---------------------------------------------------------------------------

/// Run the depth-limited BFS call graph traversal.
///
/// Uses the already-computed heuristic callers/callees as seed data and
/// optionally initializes a ClangEngine for AST-verified graph expansion.
/// Returns `None` if the traversal cannot start (e.g., no definition found).
#[cfg(feature = "clang-graph")]
fn run_bfs_phase(
    scope_path: &Path,
    base_name: &str,
    definition: &Option<DefinitionResult>,
    callers: &[CallerResult],
    callees: &[CalleeResult],
    args: &AnalyzeSymbolSourceArgs,
) -> Option<GraphOutput> {
    let def = definition.as_ref()?;

    // Attempt to initialize ClangEngine (best-effort).
    let mut engine: Option<ClangEngine> =
        find_compile_db(scope_path).and_then(|db_dir| ClangEngine::new(&db_dir).ok());

    // Determine engine eligibility based on user preference.
    match args.engine {
        EngineChoice::Clang if engine.is_none() => {
            // User forced clang but it's unavailable — skip graph.
            return None;
        }
        EngineChoice::Heuristic => {
            // User forced heuristic — drop clang engine.
            engine = None;
        }
        _ => {} // Auto: use whatever is available.
    }

    // If we have a clang engine, try to find the root symbol's USR.
    let root_usr: Option<String> = engine.as_mut().and_then(|eng| {
        eng.try_parse_file(std::path::Path::new(&def.file));
        eng.find_best_match(base_name).map(|node| node.usr.clone())
    });

    // Convert heuristic callers into the tuple format expected by bfs_call_graph.
    let h_callers: Vec<(String, u32, String)> = callers
        .iter()
        .map(|c| {
            (
                c.file.clone(),
                c.line.unwrap_or(0),
                c.context.clone().unwrap_or_default(),
            )
        })
        .collect();

    // Convert heuristic callees.
    let h_callees: Vec<(String, Option<u32>, String)> = callees
        .iter()
        .map(|c| {
            (
                c.callee.clone(),
                c.line,
                c.call_type
                    .clone()
                    .unwrap_or_else(|| "function".to_string()),
            )
        })
        .collect();

    let bfs_config = BfsConfig {
        max_depth: args.max_depth.min(2), // hard cap at 2
        max_nodes: args.max_nodes,
        max_edges: args.max_edges,
        max_callers_per_hop: args.max_callers,
        max_callees_per_hop: args.max_callees,
        prioritize: BfsPriority::CallersFirst,
    };

    let result = bfs_call_graph(
        root_usr.as_deref(),
        base_name,
        &def.file,
        def.line,
        &mut engine,
        &h_callers,
        &h_callees,
        &bfs_config,
    );

    Some(GraphOutput::from(result))
}

// ---------------------------------------------------------------------------
// Optional libclang-based enhancement (gated behind `clang-graph` feature)
// ---------------------------------------------------------------------------

/// Walk up from `start` looking for a directory containing
/// `compile_commands.json`. Returns the directory path if found.
#[cfg(feature = "clang-graph")]
fn find_compile_db(start: &Path) -> Option<std::path::PathBuf> {
    let mut dir = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };
    loop {
        if dir.join("compile_commands.json").is_file() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Attempt to validate/enhance ripgrep-based results using libclang AST
/// analysis. Returns `true` if enhancement was applied.
///
/// This is a best-effort enrichment: if libclang fails (missing
/// compile_commands.json, parse errors, etc.) we silently fall back to the
/// existing ripgrep results.
#[cfg(feature = "clang-graph")]
fn try_clang_enhance(
    scope_path: &Path,
    base_name: &str,
    definition: &Option<DefinitionResult>,
    callers: &mut Vec<CallerResult>,
    callees: &mut Vec<CalleeResult>,
    max_callers: usize,
    max_callees: usize,
) -> bool {
    // 1. Locate compile_commands.json.
    let db_dir = match find_compile_db(scope_path) {
        Some(d) => d,
        None => return false,
    };

    // 2. Initialize the loader.
    let loader = match CompileDbLoader::new(&db_dir) {
        Ok(l) => l,
        Err(_) => return false,
    };

    // 3. If we have a definition file, parse that TU for authoritative callees.
    let def_file = match definition.as_ref() {
        Some(d) => std::path::PathBuf::from(&d.file),
        None => return false,
    };

    let args = match loader.get_compile_args(&def_file) {
        Some(a) => a,
        None => return false,
    };

    let (edges, functions) = match TuParser::extract_edges_and_functions(loader.clang(), &args) {
        Ok(result) => result,
        Err(_) => return false,
    };

    // 4. Build a local graph from the definition TU.
    let mut graph = CallGraph::new();
    graph.ingest_edges(&edges);
    graph.ingest_functions(&functions);

    // 5. Find the function node matching our symbol.
    let matches = graph.find_by_name(base_name);
    if matches.is_empty() {
        return false;
    }

    // Prefer a definition over a declaration.
    let target = matches
        .iter()
        .find(|n| n.is_definition)
        .or(matches.first())
        .unwrap();
    let target_usr = target.usr.clone();

    // 6. Enhance callees: replace ripgrep-extracted callees with
    //    AST-verified direct callees.
    let clang_callees = graph.direct_callees(&target_usr);
    if !clang_callees.is_empty() {
        let mut verified: Vec<CalleeResult> = clang_callees
            .iter()
            .take(max_callees)
            .map(|node| CalleeResult {
                callee: node.display_name.clone(),
                line: if node.line > 0 { Some(node.line) } else { None },
                call_type: Some("ast".to_string()),
            })
            .collect();
        // Merge: keep AST-verified first, then unique ripgrep callees
        // that the AST didn't see (e.g. macro-expanded calls).
        let seen: HashSet<String> = verified.iter().map(|c| c.callee.clone()).collect();
        for rg_callee in callees.iter() {
            if !seen.contains(&rg_callee.callee) && verified.len() < max_callees {
                verified.push(CalleeResult {
                    callee: rg_callee.callee.clone(),
                    line: rg_callee.line,
                    call_type: rg_callee.call_type.clone(),
                });
            }
        }
        *callees = verified;
    }

    // 7. Enhance callers: add AST-verified callers, mark them.
    let clang_callers = graph.direct_callers(&target_usr);
    if !clang_callers.is_empty() {
        let existing: HashSet<String> = callers
            .iter()
            .filter_map(|c| c.line.map(|l| format!("{}:{}", c.file, l)))
            .collect();

        for node in clang_callers.iter().take(max_callers) {
            let key = format!("{}:{}", node.file, node.line);
            if !existing.contains(&key) && callers.len() < max_callers {
                callers.push(CallerResult {
                    file: node.file.clone(),
                    line: if node.line > 0 { Some(node.line) } else { None },
                    context: Some(format!("[clang] {}", node.display_name)),
                });
            }
        }
    }

    true
}

#[cfg(test)]
#[path = "analyze_symbol_source_tests.rs"]
mod tests;
