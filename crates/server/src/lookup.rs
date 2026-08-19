//! Token-at-position lookup, shared by hover / definition / document
//! highlight / completion.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use foldhash::HashMap;
use mago_database::file::File as MagoFile;
use mago_database::file::FileId;
use mago_span::Position;
use mago_syntax::lexer::Lexer;
use mago_syntax::settings::LexerSettings;
use mago_syntax::token::Token;
use mago_syntax::token::TokenKind;
use mago_syntax_core::input::Input;

/// Variable token (`$foo`) under the cursor.
///
/// Resolved name lookups go through [`mago_names::ResolvedNames`]
/// directly. Variables are not tracked there, so this byte-level scan
/// handles the only case the resolution map can't.
#[derive(Debug, Clone, Copy)]
pub struct VarAtCursor<'file> {
    /// Identifier text including the leading `$`.
    pub raw: &'file [u8],
    /// Identifier text without the leading `$`.
    pub name: &'file [u8],
    pub start: u32,
    pub end: u32,
}

/// Find the variable token (`$foo`) whose span covers `offset`. Operates
/// on file bytes directly: walks back from the cursor to the `$` and
/// forward to the end of the identifier. No lex required.
#[must_use]
pub fn variable_at_offset(file: &MagoFile, offset: u32) -> Option<VarAtCursor<'_>> {
    let bytes = file.contents.as_ref();
    let off = offset as usize;
    if off >= bytes.len() {
        return None;
    }

    let dollar = if bytes[off] == b'$' {
        off
    } else if is_var_char(bytes[off]) {
        let mut s = off;
        while s > 0 && is_var_char(bytes[s - 1]) {
            s -= 1;
        }
        if s == 0 || bytes[s - 1] != b'$' {
            return None;
        }
        s - 1
    } else {
        return None;
    };

    let name_start = dollar + 1;
    if name_start >= bytes.len() || !is_var_first_char(bytes[name_start]) {
        return None;
    }

    let mut end = name_start;
    while end < bytes.len() && is_var_char(bytes[end]) {
        end += 1;
    }

    let raw = &bytes[dollar..end];
    let name = &bytes[name_start..end];
    Some(VarAtCursor { raw, name, start: dollar as u32, end: end as u32 })
}

/// How a `::` member access names its owning class-like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberQualifier {
    /// `self::` or `static::` — the class-like enclosing the cursor.
    Enclosing,
    /// `parent::` — the enclosing class-like's direct parent.
    Parent,
    /// An explicit name (`Foo::`, `Bar\Baz::`, `\Qux::`). The payload is the
    /// start offset of the name token, so the caller can resolve it through
    /// [`mago_names::ResolvedNames`] and get the imported/aliased FQCN.
    Named(u32),
}

/// What follows the `::`, which decides how the member is looked up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberSelector {
    /// `Foo::BAR` — an enum case or a class constant.
    Constant,
    /// `Foo::bar(` — a static method.
    Method,
    /// `Foo::$bar` — a static property.
    Property,
}

/// A `::`-accessed class member under the cursor.
///
/// Purely syntactic: this reports *what was written*, not what it resolves to.
/// Turning it into a declaration is [`crate::member`]'s job, which needs the
/// codebase. Instance access (`->`) is deliberately not handled — resolving the
/// receiver needs type inference, whereas a `::` qualifier is always a name.
#[derive(Debug, Clone, Copy)]
pub struct StaticMemberAtCursor<'file> {
    /// Member name as written, without the leading `$` of a static property.
    pub name: &'file [u8],
    /// Span of the name, likewise excluding a static property's `$`.
    pub start: u32,
    pub end: u32,
    pub qualifier: MemberQualifier,
    pub selector: MemberSelector,
}

/// Find the `::`-accessed member whose name covers `offset`.
///
/// Returns `None` when the cursor isn't on the right-hand side of a `::`, or
/// when the qualifier isn't a name (`$class::CONST`, `(expr)::CONST`).
/// `Foo::class` is excluded — it's a magic constant, not a member.
#[must_use]
pub fn static_member_at_offset(file: &MagoFile, offset: u32) -> Option<StaticMemberAtCursor<'_>> {
    let tokens = lex(file);
    let index = token_at_offset(&tokens, offset)?;
    let token = tokens[index];

    let colon = previous_significant(&tokens, index)?;
    if !matches!(tokens[colon].kind, TokenKind::ColonColon) {
        return None;
    }

    let owner = previous_significant(&tokens, colon)?;
    let qualifier = match tokens[owner].kind {
        TokenKind::Self_ | TokenKind::Static => MemberQualifier::Enclosing,
        TokenKind::Parent => MemberQualifier::Parent,
        TokenKind::Identifier | TokenKind::QualifiedIdentifier | TokenKind::FullyQualifiedIdentifier => {
            MemberQualifier::Named(tokens[owner].start.offset)
        }
        _ => return None,
    };

    if matches!(token.kind, TokenKind::Variable) {
        let start = token.start.offset + 1;
        let end = token.start.offset + token.value.len() as u32;
        if end <= start {
            return None;
        }

        return Some(StaticMemberAtCursor {
            name: &token.value[1..],
            start,
            end,
            qualifier,
            selector: MemberSelector::Property,
        });
    }

    // Member names may be reserved words (`Foo::LIST`, `Foo::default`), so accept
    // any token whose text is identifier-shaped rather than filtering on kind.
    if !is_identifier(token.value) || token.value.eq_ignore_ascii_case(b"class") {
        return None;
    }

    let selector = match next_significant(&tokens, index) {
        Some(i) if matches!(tokens[i].kind, TokenKind::LeftParenthesis) => MemberSelector::Method,
        _ => MemberSelector::Constant,
    };

    Some(StaticMemberAtCursor {
        name: token.value,
        start: token.start.offset,
        end: token.start.offset + token.value.len() as u32,
        qualifier,
        selector,
    })
}

/// An identifier or `$name` token under the cursor.
#[derive(Debug, Clone, Copy)]
pub struct NameAtCursor<'file> {
    /// Token text, without the leading `$` of a variable.
    pub name: &'file [u8],
    /// Span of the name, likewise excluding a leading `$`.
    pub start: u32,
    pub end: u32,
    /// Whether the token was written with a leading `$`.
    pub is_variable: bool,
}

/// Find the identifier (or `$name`) token whose span covers `offset`.
///
/// Unlike [`variable_at_offset`] this is token-based, so it won't mistake a
/// substring of some larger construct for a name.
#[must_use]
pub fn name_at_offset(file: &MagoFile, offset: u32) -> Option<NameAtCursor<'_>> {
    let tokens = lex(file);
    let token = tokens[token_at_offset(&tokens, offset)?];

    if matches!(token.kind, TokenKind::Variable) {
        let name = token.value.get(1..).filter(|rest| !rest.is_empty())?;

        return Some(NameAtCursor {
            name,
            start: token.start.offset + 1,
            end: token.start.offset + token.value.len() as u32,
            is_variable: true,
        });
    }

    if !is_identifier(token.value) {
        return None;
    }

    Some(NameAtCursor {
        name: token.value,
        start: token.start.offset,
        end: token.start.offset + token.value.len() as u32,
        is_variable: false,
    })
}

/// Index of the token whose span covers `offset`, if any.
#[must_use]
pub fn token_at_offset(tokens: &[Token<'_>], offset: u32) -> Option<usize> {
    let index = tokens.partition_point(|t| t.start.offset <= offset).checked_sub(1)?;
    let token = tokens[index];
    if offset < token.start.offset + token.value.len() as u32 { Some(index) } else { None }
}

/// Index of the nearest non-trivia token before `index`.
#[must_use]
pub fn previous_significant(tokens: &[Token<'_>], mut index: usize) -> Option<usize> {
    while index > 0 {
        index -= 1;
        if !is_trivia(tokens[index].kind) {
            return Some(index);
        }
    }

    None
}

/// Index of the nearest non-trivia token after `index`.
#[must_use]
pub fn next_significant(tokens: &[Token<'_>], index: usize) -> Option<usize> {
    tokens.iter().enumerate().skip(index + 1).find(|(_, t)| !is_trivia(t.kind)).map(|(i, _)| i)
}

/// Is `bytes` shaped like a PHP identifier? PHP allows bytes >= 0x80 in
/// identifiers, so they're accepted too.
#[must_use]
pub fn is_identifier(bytes: &[u8]) -> bool {
    let Some((first, rest)) = bytes.split_first() else { return false };
    if !(first.is_ascii_alphabetic() || *first == b'_' || *first >= 0x80) {
        return false;
    }

    rest.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b >= 0x80)
}

fn is_var_first_char(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphabetic()
}

fn is_var_char(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

/// Lex `file` into a token vector.
///
/// Backed by the per-file [`CacheEntry`] so repeated capability calls on the
/// same file skip the state-machine lex entirely; the only per-call cost is
/// the `Vec<Token<'_>>` reconstruction from cached offsets.
#[must_use]
pub fn lex(file: &MagoFile) -> Vec<Token<'_>> {
    let entry = cached_entry(file);
    let bytes = file.contents.as_ref();
    entry
        .tokens
        .iter()
        .map(|r| Token {
            kind: r.kind,
            start: Position { offset: r.start },
            value: &bytes[r.start as usize..r.end as usize],
        })
        .collect()
}

/// Drop cached lex entries for the given files.
///
/// Called when files change so the next [`lex`] call rebuilds. The hash-check
/// path also catches stale entries, but eager invalidation prevents the cache
/// from growing with versions of the same file.
pub fn invalidate(file_ids: &[FileId]) {
    if let Ok(mut guard) = cache().lock() {
        for id in file_ids {
            guard.remove(id);
        }
    }
}

/// Returns `true` if a token is whitespace or a comment.
#[must_use]
pub fn is_trivia(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Whitespace
            | TokenKind::SingleLineComment
            | TokenKind::HashComment
            | TokenKind::MultiLineComment
            | TokenKind::DocBlockComment
    )
}

#[derive(Clone, Copy, Debug)]
struct RawToken {
    kind: TokenKind,
    start: u32,
    end: u32,
}

#[derive(Debug)]
struct CacheEntry {
    tokens: Vec<RawToken>,
}

const LEX_CACHE_CAP: usize = 1024;

type LexCache = Mutex<HashMap<FileId, (u64, Arc<CacheEntry>)>>;

fn cache() -> &'static LexCache {
    static LEX_CACHE: OnceLock<LexCache> = OnceLock::new();
    LEX_CACHE.get_or_init(|| Mutex::new(HashMap::default()))
}

fn cached_entry(file: &MagoFile) -> Arc<CacheEntry> {
    let hash = xxhash_rust::xxh3::xxh3_64(&file.contents);
    if let Ok(guard) = cache().lock()
        && let Some((h, t)) = guard.get(&file.id)
        && *h == hash
    {
        return Arc::clone(t);
    }

    let entry = Arc::new(CacheEntry { tokens: lex_uncached(file) });
    if let Ok(mut guard) = cache().lock() {
        if guard.len() >= LEX_CACHE_CAP
            && let Some(k) = guard.keys().next().copied()
        {
            guard.remove(&k);
        }
        guard.insert(file.id, (hash, Arc::clone(&entry)));
    }
    entry
}

fn lex_uncached(file: &MagoFile) -> Vec<RawToken> {
    let input = Input::new(file.id, file.contents.as_ref());
    let mut lexer = Lexer::new(input, LexerSettings::default());
    let mut out = Vec::new();
    while let Some(result) = lexer.advance() {
        if let Ok(t) = result {
            let len = t.value.len() as u32;
            out.push(RawToken { kind: t.kind, start: t.start.offset, end: t.start.offset + len });
        }
    }
    out
}
