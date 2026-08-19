//! `get_definition`: resolve the identifier under the cursor to its
//! fully-qualified name and look up that symbol's declaration span.
//!
//! Falls back to [`crate::member`] for `::`-accessed class members, which never
//! appear in the name-resolution map.

use mago_database::file::FileId;

use crate::Server;
use crate::domain::Range;
use crate::domain::SymbolLocation;

impl Server {
    /// The declaration location of the symbol whose identifier covers `offset`
    /// in `file_id`, or `None` if the offset isn't on a resolvable name.
    pub fn get_definition(&mut self, file_id: FileId, offset: u32) -> Option<SymbolLocation> {
        if let Some(analysis) = self.file_analysis_for(file_id)
            && let Some((_, _, fqcn, _)) = analysis.resolved().at_offset(offset)
            && let Some(span) = self.codebase().span_of(fqcn)
        {
            return Some(SymbolLocation { file: span.file_id, range: Range::new(span.start.offset, span.end.offset) });
        }

        let (_, member) = self.resolve_static_member(file_id, offset)?;
        let span = member.declaration;

        Some(SymbolLocation { file: span.file_id, range: Range::new(span.start.offset, span.end.offset) })
    }
}
