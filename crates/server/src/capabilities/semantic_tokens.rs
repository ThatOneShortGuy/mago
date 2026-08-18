//! `get_semantic_tokens`: token-level highlighting.
//!
//! Lexes the file and classifies each token to a [`SemanticTokenKind`], skipping
//! tokens that span multiple lines (LSP semantic tokens are single-line). Emits
//! absolute byte offsets; the protocol layer delta-encodes them.
//!
//! Semantic tokens *replace* the client's own (TextMate / tree-sitter)
//! highlighting wherever they land, so this classifier deliberately stays
//! silent on everything a client already colours at least as well. In
//! particular it emits nothing for strings and comments: those are the
//! injection points for embedded grammars — SQL in a heredoc, HTML in an
//! interpolated string, types in a PHPDoc block — and a blanket `string` or
//! `comment` token flattens all of that back to a single colour.

use mago_database::DatabaseReader;
use mago_database::file::FileId;
use mago_syntax::token::Token;
use mago_syntax::token::TokenKind;

use crate::Server;
use crate::domain::SemanticTokenItem;
use crate::domain::SemanticTokenKind;
use crate::lookup;

impl Server {
    /// Semantic-highlighting tokens for `file_id`, as absolute byte spans.
    pub fn get_semantic_tokens(&mut self, file_id: FileId) -> Vec<SemanticTokenItem> {
        let Ok(file) = self.database().get(&file_id) else {
            return Vec::new();
        };

        let tokens = lookup::lex(&file);

        // Classification needs both neighbours: `prev` tells a member name from
        // a keyword (`Builder::new()`), `next` tells a call from a bare name
        // (`up()` versus the `void` return type that follows it).
        let mut previous_kinds: Vec<Option<TokenKind>> = Vec::with_capacity(tokens.len());
        let mut seen: Option<TokenKind> = None;
        for token in &tokens {
            previous_kinds.push(seen);
            if !lookup::is_trivia(token.kind) {
                seen = Some(token.kind);
            }
        }

        let mut next_kinds: Vec<Option<TokenKind>> = vec![None; tokens.len()];
        let mut upcoming: Option<TokenKind> = None;
        for index in (0..tokens.len()).rev() {
            next_kinds[index] = upcoming;
            if !lookup::is_trivia(tokens[index].kind) {
                upcoming = Some(tokens[index].kind);
            }
        }

        let mut out: Vec<SemanticTokenItem> = Vec::with_capacity(tokens.len() / 2);
        for (index, token) in tokens.iter().enumerate() {
            let length = token.value.len() as u32;
            if length == 0 {
                continue;
            }

            let line = file.line_number(token.start.offset);
            let last_byte = token.start.offset + length;
            // LSP semantic tokens are single-line; skip tokens that wrap.
            if file.line_number(last_byte.saturating_sub(1)) != line {
                continue;
            }

            let (previous, next) = (previous_kinds[index], next_kinds[index]);
            match token.kind {
                TokenKind::QualifiedIdentifier | TokenKind::FullyQualifiedIdentifier => {
                    push_qualified_name(&mut out, token, next);
                }
                _ => {
                    if let Some(kind) = classify(token, previous, next) {
                        out.push(SemanticTokenItem { offset: token.start.offset, length, kind });
                    }
                }
            }
        }

        out
    }
}

/// Splits `Foo\Bar` into its namespace prefix and its trailing name, so the
/// class (or function) at the end keeps its own colour instead of the whole
/// path reading as one namespace.
fn push_qualified_name(out: &mut Vec<SemanticTokenItem>, token: &Token<'_>, next: Option<TokenKind>) {
    let value = token.value;
    let Some(separator) = value.iter().rposition(|byte| *byte == b'\\') else {
        return;
    };

    if separator > 0 {
        out.push(SemanticTokenItem {
            offset: token.start.offset,
            length: separator as u32,
            kind: SemanticTokenKind::Namespace,
        });
    }

    let tail_length = value.len() - separator - 1;
    if tail_length == 0 {
        return;
    }

    out.push(SemanticTokenItem {
        offset: token.start.offset + separator as u32 + 1,
        length: tail_length as u32,
        kind: if next == Some(TokenKind::LeftParenthesis) {
            SemanticTokenKind::Function
        } else {
            SemanticTokenKind::Type
        },
    });
}

#[allow(clippy::enum_glob_use)]
fn classify(token: &Token<'_>, prev: Option<TokenKind>, next: Option<TokenKind>) -> Option<SemanticTokenKind> {
    use SemanticTokenKind as K;
    use TokenKind::*;

    let is_call = next == Some(LeftParenthesis);

    // Anything after `->`, `?->` or `::` names a member, whatever the lexer
    // called it: `Builder::new()`, `$this->list`, `$config->default` all lex
    // their member as a reserved keyword.
    if matches!(prev, Some(MinusGreaterThan | QuestionMinusGreaterThan | ColonColon))
        && (token.kind == Identifier || token.kind.is_keyword())
    {
        return Some(if is_call { K::Function } else { K::Property });
    }

    Some(match token.kind {
        LiteralInteger | LiteralFloat => K::Number,
        Variable => K::Variable,
        Identifier => match prev {
            // A declaration site: the name belongs to the thing being declared.
            Some(Class | Interface | Trait | Enum | Extends | Implements | New | Instanceof | Insteadof) => K::Type,
            Some(Function) => K::Function,
            Some(Namespace | Use) => K::Namespace,
            // Otherwise only a call is unambiguous. Bare names are left to the
            // client: they are just as likely to be a type hint (`void`,
            // `int`), a constant, or a `declare`/attribute name, and guessing
            // from capitalisation loses more than it wins.
            _ => {
                if is_call {
                    K::Function
                } else {
                    return None;
                }
            }
        },
        Abstract | And | Array | As | Break | Callable | Case | Catch | Class | ClassConstant | Clone | Const
        | Continue | Declare | Default | Do | Echo | Else | ElseIf | Empty | EndDeclare | EndFor | EndForeach
        | EndIf | EndSwitch | EndWhile | Enum | Eval | Exit | Extends | False | Final | Finally | Fn | For
        | Foreach | From | Function | Global | Goto | HaltCompiler | If | Implements | Include | IncludeOnce
        | Instanceof | Insteadof | Interface | Isset | List | Match | Namespace | New | Null | Or | Parent | Print
        | Private | Protected | Public | Readonly | Require | RequireOnce | Return | Self_ | Static | Switch
        | Throw | Trait | Try | True | Unset | Use | Var | While | Xor | Yield => K::Keyword,
        TraitConstant | FunctionConstant | MethodConstant | LineConstant | FileConstant | DirConstant
        | NamespaceConstant => K::Keyword,
        Ampersand
        | AmpersandEqual
        | AmpersandAmpersand
        | AmpersandAmpersandEqual
        | Asterisk
        | AsteriskEqual
        | Bang
        | BangEqual
        | BangEqualEqual
        | Caret
        | CaretEqual
        | Colon
        | ColonColon
        | Comma
        | Dot
        | DotEqual
        | DotDotDot
        | Equal
        | EqualEqual
        | EqualEqualEqual
        | EqualGreaterThan
        | GreaterThan
        | GreaterThanEqual
        | LessThan
        | LessThanEqual
        | LessThanGreaterThan
        | LessThanEqualGreaterThan
        | Minus
        | MinusEqual
        | MinusMinus
        | MinusGreaterThan
        | Percent
        | PercentEqual
        | Pipe
        | PipeEqual
        | PipePipe
        | Plus
        | PlusEqual
        | PlusPlus
        | Question
        | QuestionQuestion
        | QuestionQuestionEqual
        | QuestionMinusGreaterThan
        | Slash
        | SlashEqual
        | Tilde
        | At => K::Operator,
        _ => return None,
    })
}
