//! `get_references`: find every reference to the symbol under the cursor.
//!
//! Resolves the symbol to its fully-qualified name, then asks each host file's
//! [`ResolvedNames`](mago_names::ResolvedNames) for matching references. Matching
//! is on resolved FQNs, so aliased imports are handled (`use Bar as Qux; Qux\G`
//! resolves to `Bar\G`). A coarse byte filter skips files that can't mention the
//! name. Variables aren't tracked by name resolution, so they fall back to a
//! same-file token scan. Only `FileType::Host` files are searched.

use std::sync::Arc;

use mago_database::DatabaseReader;
use mago_database::file::File as MagoFile;
use mago_database::file::FileId;
use mago_database::file::FileType;
use mago_syntax::token::TokenKind;

use crate::Server;
use crate::domain::Range;
use crate::domain::SymbolLocation;
use crate::lookup;

impl Server {
    /// Every reference to the symbol whose identifier covers `offset` in
    /// `file_id`. When `include_declaration` is false the declaration site
    /// itself is omitted. Variables resolve to same-file occurrences only.
    pub fn get_references(&mut self, file_id: FileId, offset: u32, include_declaration: bool) -> Vec<SymbolLocation> {
        let Ok(file) = self.database().get(&file_id) else {
            return Vec::new();
        };

        if let Some(var) = lookup::variable_at_offset(&file, offset) {
            return same_file_variable_locations(&file, var.raw, var.start, file_id);
        }

        let Some(cursor_analysis) = self.file_analysis_for(file_id) else { return Vec::new() };
        let Some((_, _, target_fqn, _)) = cursor_analysis.resolved().at_offset(offset) else { return Vec::new() };
        let target_fqn = target_fqn.to_vec();

        let local_lower = local_name(&target_fqn).to_ascii_lowercase();
        let declaration = if include_declaration { None } else { self.codebase().span_of(&target_fqn) };

        let candidates: Vec<Arc<MagoFile>> = self
            .database()
            .files()
            .filter(|f| matches!(f.file_type, FileType::Host))
            .filter(|f| might_contain(f.contents.as_ref(), &local_lower))
            .collect();

        let mut out = Vec::new();
        for arc_file in candidates {
            let Some(analysis) = self.file_analysis_for(arc_file.id) else { continue };
            let exclude = declaration.filter(|d| d.file_id == arc_file.id).map(|d| d.start.offset);
            for (start, end) in analysis.resolved().references_to(&target_fqn, exclude) {
                out.push(SymbolLocation { file: arc_file.id, range: Range::new(start, end) });
            }
        }

        out
    }
}

fn local_name(fqcn: &[u8]) -> &[u8] {
    match memchr::memrchr(b'\\', fqcn) {
        Some(i) => &fqcn[i + 1..],
        None => fqcn,
    }
}

/// Coarse case-insensitive containment pre-filter: does `haystack` possibly
/// mention `needle`? Lets the caller skip files that can't contain the symbol.
fn might_contain(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }

    if haystack.len() < needle.len() {
        return false;
    }

    let last = haystack.len() - needle.len();
    (0..=last).any(|i| haystack[i..i + needle.len()].iter().zip(needle).all(|(a, b)| a.eq_ignore_ascii_case(b)))
}

fn same_file_variable_locations(file: &MagoFile, raw: &[u8], offset: u32, file_id: FileId) -> Vec<SymbolLocation> {
    let tokens = lookup::lex(file);
    let target_scope = variable_scope(&tokens, offset);

    tokens
        .iter()
        .filter(|t| matches!(t.kind, TokenKind::Variable) && t.value == raw)
        .filter(|t| variable_scope(&tokens, t.start.offset) == target_scope)
        .map(|t| {
            let start = t.start.offset;
            let end = start + t.value.len() as u32;
            SymbolLocation { file: file_id, range: Range::new(start, end) }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VariableScope {
    Global,
    Function { start: u32, end: u32 },
}

fn variable_scope(tokens: &[mago_syntax::token::Token<'_>], offset: u32) -> VariableScope {
    let mut scope = VariableScope::Global;

    for (idx, token) in tokens.iter().enumerate() {
        if !matches!(token.kind, TokenKind::Function | TokenKind::Fn) || token.start.offset > offset {
            continue;
        }

        let Some(end) = function_scope_end(tokens, idx) else { continue };
        if offset < end {
            scope = VariableScope::Function { start: token.start.offset, end };
        }
    }

    scope
}

fn function_scope_end(tokens: &[mago_syntax::token::Token<'_>], function_idx: usize) -> Option<u32> {
    let body_start = tokens[function_idx..].iter().position(|t| matches!(t.kind, TokenKind::LeftBrace))? + function_idx;
    let mut depth = 0u32;

    for token in &tokens[body_start..] {
        match token.kind {
            TokenKind::LeftBrace => depth += 1,
            TokenKind::RightBrace => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(token.start.offset + token.value.len() as u32);
                }
            }
            _ => {}
        }
    }

    None
}
