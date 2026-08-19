//! `textDocument/rename` and `textDocument/prepareRename`.
//!
//! Symbol rename: collect every reference to the symbol under the cursor
//! (via [`crate::language_server::capabilities::references`]) and emit a
//! [`WorkspaceEdit`] that replaces each occurrence with the new name. We
//! don't try to fix up `use` statements or namespace prefixes yet, so
//! renames at the bare identifier level are safest for now.
//!
//! Names, `$variables`, and `::`-accessed class members (enum cases, class
//! constants, static methods and properties) are renameable; instance members
//! are not, because resolving `$foo->bar` to an owning class needs type
//! inference.

use foldhash::HashMap;
use tower_lsp_server::ls_types::PrepareRenameResponse;
use tower_lsp_server::ls_types::TextEdit;
use tower_lsp_server::ls_types::Uri;
use tower_lsp_server::ls_types::WorkspaceEdit;

use mago_database::file::File as MagoFile;
use mago_server::lookup;
use mago_server::member::MemberKind;

use crate::language_server::codec;
use crate::language_server::position::range_at_offsets;
use crate::language_server::state::WorkspaceState;

pub fn prepare(workspace: &mut WorkspaceState, file: &MagoFile, offset: u32) -> Option<PrepareRenameResponse> {
    let resolved_hit = workspace.file_analysis_for(file.id).and_then(|analysis| {
        analysis.resolved().at_offset(offset).map(|(start, end, fqn, _)| (start, end, fqn.to_vec()))
    });

    if let Some((start, end, fqn)) = resolved_hit {
        let fqn = fqn.as_slice();
        let local = match memchr::memrchr(b'\\', fqn) {
            Some(i) => &fqn[i + 1..],
            None => fqn,
        };
        let placeholder = String::from_utf8_lossy(local).into_owned();
        return Some(PrepareRenameResponse::RangeWithPlaceholder {
            range: range_at_offsets(file, start, end),
            placeholder,
        });
    }

    if let Some((range, member)) = workspace.server.resolve_static_member(file.id, offset) {
        return Some(PrepareRenameResponse::RangeWithPlaceholder {
            range: range_at_offsets(file, range.start, range.end),
            placeholder: String::from_utf8_lossy(&member.name).into_owned(),
        });
    }

    let var = lookup::variable_at_offset(file, offset)?;
    Some(PrepareRenameResponse::RangeWithPlaceholder {
        range: range_at_offsets(file, var.start, var.end),
        placeholder: String::from_utf8_lossy(var.raw).into_owned(),
    })
}

pub fn compute(
    workspace: &mut WorkspaceState,
    file: &MagoFile,
    offset: u32,
    new_name: String,
) -> Option<WorkspaceEdit> {
    let member = workspace.server.resolve_static_member(file.id, offset);
    let is_static_property = member.as_ref().is_some_and(|(_, m)| m.kind == MemberKind::StaticProperty);
    let is_variable = member.is_none() && lookup::variable_at_offset(file, offset).is_some();

    let replacement_name = if is_variable {
        let variable_name = new_name.strip_prefix('$').unwrap_or(&new_name);
        if !is_valid_php_identifier(variable_name) {
            return None;
        }

        format!("${variable_name}")
    } else if is_static_property {
        // Edits for a static property cover the bare name, so a typed `$` would
        // otherwise be doubled up.
        let property_name = new_name.strip_prefix('$').unwrap_or(&new_name);
        if !is_valid_php_identifier(property_name) {
            return None;
        }

        property_name.to_owned()
    } else {
        if !is_valid_php_identifier(&new_name) {
            return None;
        }

        new_name
    };

    let references = workspace.server.get_references(file.id, offset, true);
    if references.is_empty() {
        return None;
    }

    let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::default();
    for reference in references {
        if let Some(location) = codec::location(workspace.database(), &reference) {
            changes
                .entry(location.uri)
                .or_default()
                .push(TextEdit { range: location.range, new_text: replacement_name.clone() });
        }
    }

    if changes.is_empty() {
        return None;
    }

    Some(WorkspaceEdit {
        changes: Some(changes.into_iter().collect()),
        document_changes: None,
        change_annotations: None,
    })
}

fn is_valid_php_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else { return false };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }

    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}
