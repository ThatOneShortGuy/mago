//! `get_code_actions`: quickfixes from analyzer/linter autofixes, plus
//! cursor-driven refactors.
//!
//! For each issue with edits whose primary annotation overlaps the requested
//! byte range (in the requested file), emit one quickfix carrying the issue's
//! edits.
//!
//! Separately, and not driven by any issue, the function-like under the cursor
//! gets an offer to record its inferred return type as a `@return` tag. See
//! [`inferred_return_action`].

use std::fmt::Write;

use foldhash::HashMap;

use mago_allocator::LocalArena;
use mago_codex::ttype::TType;
use mago_codex::ttype::builder::get_union_from_type;
use mago_codex::ttype::resolution::TypeResolutionContext;
use mago_codex::ttype::union::TUnion;
use mago_database::DatabaseReader;
use mago_database::file::File as MagoFile;
use mago_database::file::FileId;
use mago_names::scope::NamespaceScope;
use mago_phpdoc_syntax::parser::parse_type;
use mago_reporting::Annotation;
use mago_reporting::AnnotationKind;
use mago_reporting::Issue;
use mago_reporting::Level;
use mago_span::Span;

use crate::Server;
use crate::domain::CodeActionItem;
use crate::domain::CodeActionKind;
use crate::domain::DiagnosticData;
use crate::domain::FunctionLikeSite;
use crate::domain::Range;
use crate::domain::Severity;
use crate::domain::TextReplacement;

#[derive(Default)]
struct FileWideFix {
    issue_count: usize,
    edits: Vec<TextReplacement>,
}

impl Server {
    /// Code actions for `[start, end]` of `file_id`.
    ///
    /// Ordered so the most specific actions come first: direct fixes, then
    /// `@mago-expect` suppressions, then file-wide "fix all" actions, and
    /// finally refactors, which resolve no diagnostic and so should never
    /// outrank something that does.
    ///
    /// Takes `&mut self` because the refactors consult the analyzer's
    /// expression-type index, which is built lazily per content hash.
    pub fn get_code_actions(&mut self, file_id: FileId, start: u32, end: u32) -> Vec<CodeActionItem> {
        let mut direct_actions = Vec::new();
        let mut expect_actions = Vec::new();
        let mut file_wide_fixes: HashMap<String, FileWideFix> = HashMap::default();

        if let Some(issues) = self.last_issues() {
            for issue in issues.iter() {
                collect_file_wide_fix(issue, file_id, &mut file_wide_fixes);
                if let Some(action) = action_for(issue, file_id, start, end) {
                    direct_actions.push(action);
                }
                if let Some(action) = expect_action_for(self, issue, file_id, start, end, "analysis") {
                    expect_actions.push(action);
                }
            }
        }

        for analysis in self.analyses() {
            for issue in analysis.lint_issues.iter() {
                collect_file_wide_fix(issue, file_id, &mut file_wide_fixes);
                if let Some(action) = action_for(issue, file_id, start, end) {
                    direct_actions.push(action);
                }
                if let Some(action) = expect_action_for(self, issue, file_id, start, end, "lint") {
                    expect_actions.push(action);
                }
            }
        }

        let mut file_wide_fixes: Vec<(String, FileWideFix)> =
            file_wide_fixes.into_iter().filter(|(_, fix)| fix.issue_count > 1 && !fix.edits.is_empty()).collect();
        file_wide_fixes.sort_by(|(a, _), (b, _)| a.cmp(b));

        direct_actions
            .into_iter()
            .chain(expect_actions)
            .chain(
                file_wide_fixes
                    .into_iter()
                    .map(|(code, fix)| file_action(file_id, format!("Fix all `{code}` issues in file"), fix.edits)),
            )
            .chain(inferred_return_action(self, file_id, start, end))
            .collect()
    }
}

/// Offer to record the return type inferred from a function-like's body as a
/// `@return` tag on its docblock.
///
/// The analyzer resolves call sites from *declared* signatures and never from
/// bodies, so a method with no `@return` is `array` (or `mixed`) to every
/// caller no matter how precisely its body is understood. This action is the
/// bridge: it takes what the analyzer already worked out while checking the
/// body and offers it to the author as a declaration they can accept, edit, or
/// ignore. Nothing is inferred behind their back; the contract only changes
/// when a human writes it down.
///
/// Deliberately not offered when an `@return` is already present. Silently
/// rewriting a contract someone wrote by hand is the action-at-a-distance this
/// design exists to avoid, and narrowing an over-wide declaration is already
/// covered by the analyzer's `overly-wide-return-type` fix.
fn inferred_return_action(server: &mut Server, file_id: FileId, start: u32, end: u32) -> Option<CodeActionItem> {
    let analysis = server.file_analysis_for(file_id)?;
    let site = innermost_function_like(&analysis.function_like_sites, start, end)?.clone();

    let inferred = server.type_index_for(file_id)?.inferred_returns_by_span.get(&site.span)?;
    let rendered = render_docblock_type(inferred)?;

    let file = server.database().get(&file_id).ok()?;
    let replacement = return_tag_edit(&file, &site, &rendered)?;

    let mut edits = HashMap::default();
    edits.insert(file_id, vec![replacement]);

    Some(CodeActionItem {
        title: format!("Document inferred return type: @return {}", abbreviate(&rendered)),
        edits,
        diagnostic: None,
        kind: CodeActionKind::RefactorRewrite,
    })
}

/// The tightest function-like declaration wholly containing `[start, end]`.
///
/// Innermost rather than first so that a cursor inside a method of a class
/// resolves to the method, and a nested declaration wins over its parent.
fn innermost_function_like(sites: &[FunctionLikeSite], start: u32, end: u32) -> Option<&FunctionLikeSite> {
    sites.iter().filter(|site| site.span.0 <= start && end <= site.span.1).min_by_key(|site| site.span.1 - site.span.0)
}

/// Render `ty` as text fit for a `@return` tag, or `None` when it says nothing
/// worth writing down.
///
/// [`TType::get_id`] is documented as a form for "error messages or debugging",
/// and it is not PHPDoc source: a literal string renders as `string('pro')`,
/// which the type parser accepts but reads back as plain `string`, and a
/// literal nested in a shape renders as `array{'a': int(1)}`, which it rejects
/// outright. Emitting either would quietly weaken or destroy the very type the
/// action exists to record.
///
/// So nothing is offered on trust. Each candidate is parsed back into a
/// [`TUnion`] and compared with the type it came from, and only a candidate
/// that reproduces it exactly is used. Candidates are tried most precise first:
/// the literal-corrected rendering, then the raw rendering, then the same two
/// with literals widened away. When none survives, no action is offered at all
/// — a docblock that lies about the type is worse than no docblock.
fn render_docblock_type(ty: &TUnion) -> Option<String> {
    if ty.is_mixed() {
        return None;
    }

    if let Some(text) = faithful_rendering(ty) {
        return Some(text);
    }

    // A precise type that cannot be expressed is still worth something widened:
    // `@return list<string>` beats saying nothing about a `list{'a', 'b'}`.
    let mut widened = ty.clone();
    widened.widen_literals();

    faithful_rendering(&widened)
}

/// The most precise rendering of `ty` that reads back as `ty`, if either
/// candidate does.
fn faithful_rendering(ty: &TUnion) -> Option<String> {
    let rendered = ty.get_id().to_string();

    std::iter::once(phpdoc_literal_form(&rendered))
        .flatten()
        .chain(std::iter::once(rendered))
        .find(|candidate| reads_back_as(candidate, ty))
}

/// Whether `text`, parsed as a PHPDoc type, yields exactly `expected`.
///
/// This is the guarantee the whole rendering path rests on. Parsing alone is
/// not enough: `string('pro')` parses cleanly and silently degrades to
/// `string`, so fidelity has to be checked against the type itself rather than
/// against the parser's willingness to accept the text.
fn reads_back_as(text: &str, expected: &TUnion) -> bool {
    let arena = LocalArena::new();
    let Ok(parsed) = parse_type(&arena, text.as_bytes(), Span::zero()) else {
        return false;
    };

    let Ok(reparsed) = get_union_from_type(&parsed, &NamespaceScope::global(), &TypeResolutionContext::default(), None)
    else {
        return false;
    };

    reparsed.get_id() == expected.get_id()
}

/// Rewrite the literal-scalar wrappers `get_id` emits into the source syntax
/// PHPDoc actually uses: `string('pro')` to `'pro'`, `int(3)` to `3`, and
/// `float(0.5)` to `0.5`.
///
/// Returns `None` when there is nothing to rewrite, so the caller does not test
/// the same candidate twice.
///
/// `get_id` does not escape quotes inside a literal string, so its output is
/// genuinely ambiguous for a string containing `')`. This scans quoted runs as
/// opaque and gives up rather than guessing; anything it still gets wrong is
/// caught by [`reads_back_as`] and degrades to a widened rendering rather than
/// reaching the user's file.
fn phpdoc_literal_form(rendered: &str) -> Option<String> {
    let bytes = rendered.as_bytes();
    let mut out = String::with_capacity(rendered.len());
    let mut index = 0;
    let mut rewrote = false;

    while index < bytes.len() {
        // A quoted run is the author's data, not type syntax: copy it verbatim
        // so a shape key like `'int('` is never read as a wrapper.
        if bytes[index] == b'\'' {
            let end = index + 1 + rendered[index + 1..].find('\'')? + 1;
            out.push_str(&rendered[index..end]);
            index = end;
            continue;
        }

        if let Some((content, after)) = literal_wrapper_at(rendered, index) {
            out.push_str(content);
            index = after;
            rewrote = true;
            continue;
        }

        let character = rendered[index..].chars().next()?;
        out.push(character);
        index += character.len_utf8();
    }

    rewrote.then_some(out)
}

/// The source text and end offset of the literal-scalar wrapper starting at
/// `index`, if one starts there.
fn literal_wrapper_at(rendered: &str, index: usize) -> Option<(&str, usize)> {
    // Only a token boundary starts a wrapper, so the `string(` tail of
    // `lowercase-string(` is not mistaken for one.
    if index > 0 && is_type_name_byte(rendered.as_bytes()[index - 1]) {
        return None;
    }

    let rest = &rendered[index..];

    // A literal string keeps its quotes. The terminator is matched as the pair
    // `')` so a `)` *inside* the literal does not end it early.
    if let Some(inner) = rest.strip_prefix("string('") {
        let close = inner.find("')")?;
        let content_end = index + "string('".len() + close + 1;

        return Some((&rendered[index + "string(".len()..content_end], content_end + 1));
    }

    for wrapper in ["int(", "float("] {
        if let Some(inner) = rest.strip_prefix(wrapper) {
            let close = inner.find(')')?;
            let content_start = index + wrapper.len();

            return Some((&rendered[content_start..content_start + close], content_start + close + 1));
        }
    }

    None
}

/// Whether `byte` can appear inside a PHPDoc type name, used to tell a real
/// `int(` wrapper from the tail of a longer name.
const fn is_type_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' || byte == b'\\'
}

/// Build the edit that records `@return {rendered}` for `site`, or `None` when
/// the declaration already documents a return type.
fn return_tag_edit(file: &MagoFile, site: &FunctionLikeSite, rendered: &str) -> Option<TextReplacement> {
    let declaration_line = file.line_number(site.span.0);
    let declaration_line_start = file.get_line_start_offset(declaration_line)?;
    let indent = line_indent(&file.contents[declaration_line_start as usize..]);

    let Some((docblock_start, docblock_end)) = site.docblock else {
        // No docblock at all: introduce one on its own line above the
        // declaration, matching the declaration's indentation.
        return Some(TextReplacement {
            range: Range::new(declaration_line_start, declaration_line_start),
            new_text: format!("{indent}/** @return {rendered} */\n"),
        });
    };

    let docblock = file.contents.get(docblock_start as usize..docblock_end as usize)?;
    if contains_return_tag(docblock) {
        return None;
    }

    let docblock = std::str::from_utf8(docblock).ok()?;
    match docblock.rfind('\n') {
        // Multi-line: splice a tag line in ahead of the closing delimiter,
        // taking the star column from the closing line so the block stays
        // aligned however the author indented it.
        Some(last_newline) => {
            let closing_line_start = docblock_start + (last_newline as u32) + 1;
            let closing_indent = line_indent(&file.contents[closing_line_start as usize..]);

            Some(TextReplacement {
                range: Range::new(closing_line_start, closing_line_start),
                new_text: format!("{closing_indent}* @return {rendered}\n"),
            })
        }
        // Single-line: a tag has to start its own line to parse, so the block
        // is reflowed rather than appended to.
        None => {
            let body = docblock.strip_prefix("/**")?.strip_suffix("*/")?.trim();
            let mut expanded = format!("{indent}/**\n");
            if !body.is_empty() {
                let _ = writeln!(expanded, "{indent} * {body}");
            }
            let _ = write!(expanded, "{indent} * @return {rendered}\n{indent} */");

            Some(TextReplacement { range: Range::new(docblock_start, docblock_end), new_text: expanded })
        }
    }
}

/// Whether a docblock already carries a `@return` tag.
///
/// Matched on a word boundary so `@returns` (which mago does not honour) and
/// prose mentioning `@return` inside a longer word do not count as one.
fn contains_return_tag(docblock: &[u8]) -> bool {
    let mut haystack = docblock;
    while let Some(at) = memchr::memmem::find(haystack, b"@return") {
        let after = at + b"@return".len();
        match haystack.get(after) {
            None => return true,
            Some(byte) if !byte.is_ascii_alphanumeric() && *byte != b'-' && *byte != b'_' => return true,
            Some(_) => haystack = &haystack[after..],
        }
    }

    false
}

/// Shorten a rendered type for use in an action title, which editors show on a
/// single line.
fn abbreviate(rendered: &str) -> String {
    const LIMIT: usize = 60;

    if rendered.len() <= LIMIT {
        return rendered.to_string();
    }

    let mut cut = LIMIT;
    while cut > 0 && !rendered.is_char_boundary(cut) {
        cut -= 1;
    }

    format!("{}...", &rendered[..cut])
}

fn file_action(file_id: FileId, title: String, edits: Vec<TextReplacement>) -> CodeActionItem {
    let mut grouped = HashMap::default();
    grouped.insert(file_id, edits);

    CodeActionItem { title, edits: grouped, diagnostic: None, kind: CodeActionKind::QuickFix }
}

fn collect_file_wide_fix(issue: &Issue, file_id: FileId, fixes: &mut HashMap<String, FileWideFix>) {
    let Some(code) = issue.code.as_deref() else {
        return;
    };

    let Some(file_edits) = issue.edits.get(&file_id) else {
        return;
    };

    if file_edits.is_empty() {
        return;
    }

    let fix = fixes.entry(code.to_string()).or_default();
    fix.issue_count += 1;
    fix.edits.extend(file_edits.iter().map(|edit| TextReplacement {
        range: Range::new(edit.range.start, edit.range.end),
        new_text: String::from_utf8_lossy(&edit.new_text).into_owned(),
    }));
}

fn expect_action_for(
    server: &Server,
    issue: &Issue,
    file_id: FileId,
    start: u32,
    end: u32,
    category: &str,
) -> Option<CodeActionItem> {
    if !overlaps(issue, file_id, start, end) {
        return None;
    }

    let code = issue.code.as_deref()?;
    let primary = primary_annotation(issue)?;
    let file = server.database().get(&file_id).ok()?;
    let line = file.line_number(primary.span.start.offset);
    let line_start = file.get_line_start_offset(line)?;
    let indent = line_indent(&file.contents[line_start as usize..]);
    let qualified_code = format!("{category}:{code}");
    let new_text = format!("{indent}/** @mago-expect {qualified_code} */\n");

    let mut edits = HashMap::default();
    edits.insert(file_id, vec![TextReplacement { range: Range::new(line_start, line_start), new_text }]);

    let diagnostic = primary_annotation(issue).map(|primary| DiagnosticData {
        file: primary.span.file_id,
        range: Range::new(primary.span.start.offset, primary.span.end.offset),
        severity: level_to_severity(issue.level),
        code: issue.code.clone(),
        message: issue.message.clone(),
    });

    Some(CodeActionItem {
        title: format!("Add @mago-expect {qualified_code}"),
        edits,
        diagnostic,
        kind: CodeActionKind::QuickFix,
    })
}

fn line_indent(line: &[u8]) -> String {
    line.iter().take_while(|b| matches!(b, b' ' | b'\t')).map(|b| *b as char).collect()
}

/// Build a code action for `issue` if it has edits and its primary annotation
/// overlaps `[start, end]` in `file_id`.
fn action_for(issue: &Issue, file_id: FileId, start: u32, end: u32) -> Option<CodeActionItem> {
    if issue.edits.is_empty() || !overlaps(issue, file_id, start, end) {
        return None;
    }

    let mut edits: HashMap<FileId, Vec<TextReplacement>> = HashMap::default();
    for (edit_file_id, file_edits) in &issue.edits {
        let replacements: Vec<TextReplacement> = file_edits
            .iter()
            .map(|edit| TextReplacement {
                range: Range::new(edit.range.start, edit.range.end),
                new_text: String::from_utf8_lossy(&edit.new_text).into_owned(),
            })
            .collect();

        if !replacements.is_empty() {
            edits.entry(*edit_file_id).or_default().extend(replacements);
        }
    }

    if edits.is_empty() {
        return None;
    }

    let title = issue.help.clone().unwrap_or_else(|| format!("Apply mago fix: {}", issue.message));
    let diagnostic = primary_annotation(issue).map(|primary| DiagnosticData {
        file: primary.span.file_id,
        range: Range::new(primary.span.start.offset, primary.span.end.offset),
        severity: level_to_severity(issue.level),
        code: issue.code.clone(),
        message: issue.message.clone(),
    });

    Some(CodeActionItem { title, edits, diagnostic, kind: CodeActionKind::QuickFix })
}

/// Whether `issue`'s primary annotation is in `file_id` and overlaps `[start, end]`.
fn overlaps(issue: &Issue, file_id: FileId, start: u32, end: u32) -> bool {
    let Some(primary) = primary_annotation(issue) else {
        return false;
    };

    primary.span.file_id == file_id && primary.span.start.offset <= end && start <= primary.span.end.offset
}

fn primary_annotation(issue: &Issue) -> Option<&Annotation> {
    issue.annotations.iter().find(|a| matches!(a.kind, AnnotationKind::Primary)).or_else(|| issue.annotations.first())
}

const fn level_to_severity(level: Level) -> Severity {
    match level {
        Level::Error => Severity::Error,
        Level::Warning => Severity::Warning,
        Level::Help => Severity::Hint,
        Level::Note => Severity::Information,
    }
}

#[cfg(test)]
mod tests {
    use super::phpdoc_literal_form;

    #[test]
    fn rewrites_each_literal_wrapper() {
        assert_eq!(phpdoc_literal_form("string('pro')").as_deref(), Some("'pro'"));
        assert_eq!(phpdoc_literal_form("int(3)").as_deref(), Some("3"));
        assert_eq!(phpdoc_literal_form("float(0.5)").as_deref(), Some("0.5"));
        assert_eq!(phpdoc_literal_form("int(-7)").as_deref(), Some("-7"));
    }

    #[test]
    fn rewrites_every_member_of_a_union() {
        assert_eq!(phpdoc_literal_form("null|string('plus')|string('pro')").as_deref(), Some("null|'plus'|'pro'"));
    }

    #[test]
    fn rewrites_literals_nested_in_containers() {
        assert_eq!(
            phpdoc_literal_form("array{'family': string('pro'), 'tier': int(1)}").as_deref(),
            Some("array{'family': 'pro', 'tier': 1}")
        );
        assert_eq!(phpdoc_literal_form("list{int(1), int(2)}").as_deref(), Some("list{1, 2}"));
    }

    #[test]
    fn leaves_types_without_literals_alone() {
        // `None` rather than an unchanged copy, so the caller does not test the
        // same candidate twice.
        assert_eq!(phpdoc_literal_form("array<array-key, mixed>"), None);
        assert_eq!(phpdoc_literal_form("null|string"), None);
        assert_eq!(phpdoc_literal_form("array{'ok': bool}"), None);
    }

    #[test]
    fn does_not_mistake_a_longer_type_name_for_a_wrapper() {
        // A wrapper only starts at a token boundary; these merely end in one.
        assert_eq!(phpdoc_literal_form("non-empty-string"), None);
        assert_eq!(phpdoc_literal_form("list<lowercase-string>"), None);
    }

    #[test]
    fn treats_quoted_runs_as_opaque() {
        // A shape key is the author's data: `int(` inside it is not syntax.
        assert_eq!(phpdoc_literal_form("array{'int(': bool}"), None);
        assert_eq!(phpdoc_literal_form("array{'float(x)': string('a')}").as_deref(), Some("array{'float(x)': 'a'}"));
    }

    #[test]
    fn survives_a_parenthesis_inside_a_literal_string() {
        // The terminator is the pair `')`, so the `)` in `f(x)` does not end it.
        assert_eq!(phpdoc_literal_form("string('f(x)')").as_deref(), Some("'f(x)'"));
    }

    #[test]
    fn gives_up_on_an_unterminated_quote_rather_than_guessing() {
        // `get_id` does not escape quotes, so this is genuinely ambiguous.
        assert_eq!(phpdoc_literal_form("string('unterminated"), None);
    }
}
