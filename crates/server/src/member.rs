//! Resolving `::`-accessed class members to their declarations.
//!
//! Name resolution ([`mago_names::ResolvedNames`]) only knows about *names*:
//! class-likes, functions, and global constants. The right-hand side of a `::`
//! is a member selector, so it never appears there — which is why hover,
//! go-to-definition, references, and rename all used to come up empty on an
//! enum case or a class constant. This module fills that gap.
//!
//! # Identity is the declaration span
//!
//! The populator flattens inherited constants, methods, and properties into
//! every descendant's metadata, but the *cloned* entries keep the span of the
//! original declaration. That makes the declaration's name span a natural
//! identity for a member: two occurrences refer to the same member exactly when
//! they resolve to the same span. Comparing spans gets inheritance, interface
//! constants, and trait members right without walking the hierarchy by hand.
//!
//! # Scope
//!
//! Only `::` access is handled. An instance access (`$foo->bar`) needs the
//! receiver's inferred type to name an owner, which is a different problem with
//! different failure modes; a `::` qualifier is always `self`, `static`,
//! `parent`, or a resolvable name.

use mago_codex::metadata::class_like::ClassLikeMetadata;
use mago_codex::metadata::function_like::FunctionLikeMetadata;
use mago_database::DatabaseReader;
use mago_database::file::FileId;
use mago_database::file::FileType;
use mago_span::Position;
use mago_span::Span;
use mago_word::Word;
use mago_word::word;

use crate::Server;
use crate::domain::Range;
use crate::domain::SymbolLocation;
use crate::lookup;
use crate::lookup::MemberQualifier;
use crate::lookup::MemberSelector;

/// The kind of class member a cursor resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    EnumCase,
    ClassConstant,
    StaticMethod,
    StaticProperty,
}

impl MemberKind {
    /// PHP matches method names case-insensitively; every other member kind is
    /// case-sensitive.
    #[must_use]
    pub const fn is_case_insensitive(self) -> bool {
        matches!(self, Self::StaticMethod)
    }

    /// The keyword to show for this kind in hover text.
    #[must_use]
    pub const fn keyword(self) -> &'static str {
        match self {
            Self::EnumCase => "case",
            Self::ClassConstant => "const",
            Self::StaticMethod => "static function",
            Self::StaticProperty => "static",
        }
    }
}

/// A `::`-accessed member the cursor resolved to.
#[derive(Debug, Clone)]
pub struct MemberTarget {
    pub kind: MemberKind,
    /// Member name as declared, without the `$` of a static property.
    pub name: Vec<u8>,
    /// Span of the member's name at its declaration site.
    ///
    /// Doubles as the member's identity — see the module docs.
    pub declaration: Span,
    /// FQCN of the class-like that declares the member, when it could be
    /// determined.
    pub class: Option<Word>,
}

impl Server {
    /// Resolve the `::`-accessed member under the cursor, whether the cursor is
    /// on a use site (`Series::EnergyPrime`) or on the declaration itself
    /// (`case EnergyPrime`).
    ///
    /// Returns the range of the name under the cursor alongside the member it
    /// resolves to.
    pub fn resolve_static_member(&mut self, file_id: FileId, offset: u32) -> Option<(Range, MemberTarget)> {
        if let Some(found) = self.member_declaration_at(file_id, offset) {
            return Some(found);
        }

        let file = self.database().get(&file_id).ok()?;
        let cursor = lookup::static_member_at_offset(&file, offset)?;
        let owner = self.qualifier_owner(file_id, offset, cursor.qualifier)?;
        let target = self.member_target(&owner, cursor.name, cursor.selector)?;
        if !self.declaration_spells(&target) {
            return None;
        }

        Some((Range::new(cursor.start, cursor.end), target))
    }

    /// Every occurrence of `target` across the workspace's host files.
    ///
    /// Each candidate occurrence is resolved independently and kept only when it
    /// lands on the same declaration, so `self::X`, `Child::X`, and `Parent::X`
    /// all collapse onto one member while an unrelated class's same-named member
    /// is left alone.
    pub fn member_references(&mut self, target: &MemberTarget, include_declaration: bool) -> Vec<SymbolLocation> {
        let candidates: Vec<(FileId, Vec<(u32, u32)>)> = self
            .database()
            .files()
            .filter(|file| matches!(file.file_type, FileType::Host))
            .map(|file| (file.id, name_occurrences(file.contents.as_ref(), &target.name, target.kind)))
            .filter(|(_, hits)| !hits.is_empty())
            .collect();

        let mut out = Vec::new();
        for (file_id, hits) in candidates {
            for (start, end) in hits {
                let Some((range, found)) = self.resolve_static_member(file_id, start) else { continue };
                if found.declaration != target.declaration {
                    continue;
                }

                // A hit inside a longer identifier resolves to that identifier,
                // not to our member; drop anything that didn't land exactly.
                if range.start != start || range.end != end {
                    continue;
                }

                let is_declaration = file_id == target.declaration.file_id && start == target.declaration.start.offset;
                if is_declaration && !include_declaration {
                    continue;
                }

                out.push(SymbolLocation { file: file_id, range });
            }
        }

        out
    }

    /// The member declared at `offset`, when the cursor is sitting on a `case`,
    /// `const`, static method, or static property name in a class body.
    ///
    /// Matching is by name first, then by span. Position alone isn't enough: an
    /// enum's synthesized `cases`/`from`/`tryFrom` methods borrow the enum's own
    /// name span, so a purely positional scan would claim any cursor inside the
    /// enum.
    fn member_declaration_at(&self, file_id: FileId, offset: u32) -> Option<(Range, MemberTarget)> {
        let file = self.database().get(&file_id).ok()?;
        let cursor = lookup::name_at_offset(&file, offset)?;
        let class = self.enclosing_class_like(file_id, offset)?;
        let codebase = self.codebase();
        let metadata = codebase.get_class_like(class.as_bytes())?;

        if cursor.is_variable {
            let property = metadata.properties.get(&word([b"$", cursor.name].concat()))?;
            if !property.flags.is_static() {
                return None;
            }

            let declaration = strip_dollar(property.name_span?);
            if !covers(declaration, file_id, offset) {
                return None;
            }

            let target = MemberTarget {
                kind: MemberKind::StaticProperty,
                name: cursor.name.to_vec(),
                declaration,
                class: Some(metadata.name),
            };

            return Some((range_of(declaration), target));
        }

        let name = word(cursor.name);
        if let Some(case) = metadata.enum_cases.get(&name)
            && covers(case.name_span, file_id, offset)
        {
            let target = MemberTarget {
                kind: MemberKind::EnumCase,
                name: cursor.name.to_vec(),
                declaration: case.name_span,
                class: Some(metadata.name),
            };

            return Some((range_of(case.name_span), target));
        }

        if let Some(constant) = metadata.constants.get(&name) {
            // `ClassLikeConstantMetadata::span` is the `NAME = value` item, so it
            // starts exactly at the name.
            let declaration = name_span_at(constant.span.file_id, constant.span.start.offset, cursor.name.len());
            if covers(declaration, file_id, offset) {
                let target = MemberTarget {
                    kind: MemberKind::ClassConstant,
                    name: cursor.name.to_vec(),
                    declaration,
                    class: Some(metadata.name),
                };

                return Some((range_of(declaration), target));
            }
        }

        let method = codebase.get_method(class.as_bytes(), cursor.name)?;
        if !is_static_method(method) {
            return None;
        }

        let declaration = method.name_span?;
        if !covers(declaration, file_id, offset) {
            return None;
        }

        let target = MemberTarget {
            kind: MemberKind::StaticMethod,
            name: method.original_name.as_bytes().to_vec(),
            declaration,
            class: Some(metadata.name),
        };

        Some((range_of(declaration), target))
    }

    /// FQCN the `::` qualifier names.
    fn qualifier_owner(&mut self, file_id: FileId, offset: u32, qualifier: MemberQualifier) -> Option<Vec<u8>> {
        match qualifier {
            MemberQualifier::Enclosing => {
                self.enclosing_class_like(file_id, offset).map(|name| name.as_bytes().to_vec())
            }
            MemberQualifier::Parent => {
                let enclosing = self.enclosing_class_like(file_id, offset)?;
                let parent = self.codebase().get_class_like(enclosing.as_bytes())?.direct_parent_class?;

                Some(parent.as_bytes().to_vec())
            }
            MemberQualifier::Named(at) => {
                let analysis = self.file_analysis_for(file_id)?;
                let (_, _, fqcn, _) = analysis.resolved().at_offset(at)?;

                Some(fqcn.to_vec())
            }
        }
    }

    /// Look `name` up as a member of `owner`, following inheritance.
    fn member_target(&self, owner: &[u8], name: &[u8], selector: MemberSelector) -> Option<MemberTarget> {
        let codebase = self.codebase();
        match selector {
            MemberSelector::Method => {
                let method = codebase.get_declaring_method(owner, name)?;
                if !is_static_method(method) {
                    return None;
                }

                let declaration = method.name_span?;
                Some(MemberTarget {
                    kind: MemberKind::StaticMethod,
                    name: method.original_name.as_bytes().to_vec(),
                    class: codebase.get_declaring_method_class(owner, name),
                    declaration,
                })
            }
            MemberSelector::Property => {
                let key = [b"$", name].concat();
                let property = codebase.get_declaring_property(owner, &key)?;
                if !property.flags.is_static() {
                    return None;
                }

                let declaration = strip_dollar(property.name_span?);
                Some(MemberTarget {
                    kind: MemberKind::StaticProperty,
                    name: name.to_vec(),
                    class: codebase.get_declaring_property_class(owner, &key),
                    declaration,
                })
            }
            MemberSelector::Constant => {
                if let Some(case) = codebase.get_enum_case(owner, name) {
                    return Some(MemberTarget {
                        kind: MemberKind::EnumCase,
                        name: name.to_vec(),
                        declaration: case.name_span,
                        class: self.class_like_declaring(case.name_span).map(|meta| meta.name),
                    });
                }

                let constant = codebase.get_class_constant(owner, name)?;
                let declaration = name_span_at(constant.span.file_id, constant.span.start.offset, name.len());

                Some(MemberTarget {
                    kind: MemberKind::ClassConstant,
                    name: name.to_vec(),
                    class: self.class_like_declaring(declaration).map(|meta| meta.name),
                    declaration,
                })
            }
        }
    }

    /// Does the declaration span actually spell the member's name?
    ///
    /// Guards against metadata whose span doesn't point at a real declaration of
    /// this member — an enum's synthesized `from`/`cases`/`tryFrom` methods reuse
    /// the enum's name span, and renaming those would rewrite call sites with no
    /// declaration to match.
    fn declaration_spells(&self, target: &MemberTarget) -> bool {
        let Ok(file) = self.database().get(&target.declaration.file_id) else { return false };
        let start = target.declaration.start.offset as usize;
        let end = target.declaration.end.offset as usize;
        let Some(text) = file.contents.get(start..end) else { return false };

        if target.kind.is_case_insensitive() { text.eq_ignore_ascii_case(&target.name) } else { text == target.name }
    }

    /// The innermost class-like whose body covers `offset` in `file_id`.
    fn enclosing_class_like(&self, file_id: FileId, offset: u32) -> Option<Word> {
        self.innermost_class_like(file_id, offset, offset).map(|meta| meta.name)
    }

    /// The innermost class-like containing `span`, used to name the owner of a
    /// declaration found by span.
    fn class_like_declaring(&self, span: Span) -> Option<&ClassLikeMetadata> {
        self.innermost_class_like(span.file_id, span.start.offset, span.end.offset)
    }

    fn innermost_class_like(&self, file_id: FileId, start: u32, end: u32) -> Option<&ClassLikeMetadata> {
        self.codebase()
            .class_likes
            .values()
            .filter(|meta| meta.span.file_id == file_id)
            .filter(|meta| meta.span.start.offset <= start && end <= meta.span.end.offset)
            .min_by_key(|meta| meta.span.end.offset - meta.span.start.offset)
    }
}

/// Byte ranges in `haystack` where `name` appears as a whole identifier.
///
/// A cheap pre-filter: it over-reports (a match may be an unrelated symbol) but
/// never under-reports, and callers confirm each hit by resolving it.
fn name_occurrences(haystack: &[u8], name: &[u8], kind: MemberKind) -> Vec<(u32, u32)> {
    if name.is_empty() || haystack.len() < name.len() {
        return Vec::new();
    }

    let matches = |window: &[u8]| {
        if kind.is_case_insensitive() { window.eq_ignore_ascii_case(name) } else { window == name }
    };

    let mut out = Vec::new();
    for start in 0..=haystack.len() - name.len() {
        let end = start + name.len();
        if !matches(&haystack[start..end]) {
            continue;
        }

        // Reject matches that are part of a longer identifier.
        if start > 0 && is_identifier_byte(haystack[start - 1]) {
            continue;
        }

        if haystack.get(end).is_some_and(|b| is_identifier_byte(*b)) {
            continue;
        }

        out.push((start as u32, end as u32));
    }

    out
}

/// Methods record `static` on their [`MethodMetadata`], not in the shared
/// `MetadataFlags` (where the `STATIC` bit means `@method static` instead).
fn is_static_method(method: &FunctionLikeMetadata) -> bool {
    method.method_metadata.as_ref().is_some_and(|metadata| metadata.is_static)
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80
}

fn covers(span: Span, file_id: FileId, offset: u32) -> bool {
    span.file_id == file_id && span.start.offset <= offset && offset < span.end.offset
}

fn range_of(span: Span) -> Range {
    Range::new(span.start.offset, span.end.offset)
}

fn name_span_at(file_id: FileId, start: u32, length: usize) -> Span {
    Span::new(file_id, Position { offset: start }, Position { offset: start + length as u32 })
}

/// Narrow a `$name` span to just `name`.
fn strip_dollar(span: Span) -> Span {
    Span::new(span.file_id, Position { offset: span.start.offset + 1 }, span.end)
}
