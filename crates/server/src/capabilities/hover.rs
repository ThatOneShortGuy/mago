//! `get_context`: resolve the identifier (or variable, or `::`-accessed class
//! member) under the cursor and render a Markdown summary of it for hover.

use std::fmt::Write;

use mago_bytes::BytesDisplay;
use mago_codex::metadata::CodebaseMetadata;
use mago_codex::metadata::class_like::ClassLikeMetadata;
use mago_codex::metadata::function_like::FunctionLikeMetadata;
use mago_codex::symbol::SymbolKind;
use mago_codex::ttype::TType;
use mago_database::DatabaseReader;
use mago_database::file::FileId;

use crate::Server;
use crate::domain::HoverInfo;
use crate::domain::Range;
use crate::lookup;
use crate::member::MemberKind;
use crate::member::MemberTarget;

impl Server {
    /// Hover context for the token covering `offset` in `file_id`: rendered
    /// markdown plus the token's span. Resolves named symbols against the
    /// codebase, and falls back to a plain summary for `$variable`s.
    pub fn get_context(&mut self, file_id: FileId, offset: u32) -> Option<HoverInfo> {
        let file = self.database().get(&file_id).ok()?;
        let analysis = self.file_analysis_for(file_id)?;

        if let Some((start, end, fqcn, _)) = analysis.resolved().at_offset(offset) {
            let markdown = render(self.codebase(), fqcn)?;
            return Some(HoverInfo { markdown, range: Range::new(start, end) });
        }

        if let Some((range, member)) = self.resolve_static_member(file_id, offset) {
            let markdown = render_member(self.codebase(), &member);
            return Some(HoverInfo { markdown, range });
        }

        let var = lookup::variable_at_offset(&file, offset)?;
        let name = var.name.to_vec();
        let start = var.start;
        let end = var.end;
        let ty =
            self.type_index_for(file_id).and_then(|index| index.display_by_span.get(&(start, end))).map(String::as_str);
        Some(HoverInfo { markdown: render_variable(&name, ty), range: Range::new(start, end) })
    }
}

/// Render a `::`-accessed member: its declaring class, keyword, and name, plus
/// a signature or value when the metadata carries one.
fn render_member(codebase: &CodebaseMetadata, member: &MemberTarget) -> String {
    let owner =
        member.class.map(|class| codebase.get_class_like(class.as_bytes()).map_or(class, |meta| meta.original_name));

    let mut out = String::from("```php\n");
    if member.kind == MemberKind::StaticMethod
        && let Some(owner) = owner
        && let Some(meta) = codebase.get_declaring_method(owner.as_bytes(), &member.name)
    {
        return render_function_like(meta, Some(owner.as_bytes()));
    }

    out.push_str(member.kind.keyword());
    out.push(' ');
    if let Some(owner) = owner {
        let _ = write!(out, "{}::", BytesDisplay(owner.as_bytes()));
    }

    if member.kind == MemberKind::StaticProperty {
        out.push('$');
    }

    let _ = write!(out, "{}", BytesDisplay(&member.name));

    if member.kind == MemberKind::EnumCase
        && let Some(owner) = owner
        && let Some(case) = codebase.get_enum_case(owner.as_bytes(), &member.name)
        && let Some(value) = case.value_type.as_ref()
    {
        let _ = write!(out, " = {}", BytesDisplay(value.get_id().as_bytes()));
    }

    out.push_str("\n```");
    out
}

fn render_variable(name: &[u8], ty: Option<&str>) -> String {
    match ty {
        Some(ty) => format!("_@var_ `{}` `${}`", ty, BytesDisplay(name)),
        None => format!("_@var_ ${}", BytesDisplay(name)),
    }
}

fn render(codebase: &CodebaseMetadata, fqcn: &[u8]) -> Option<String> {
    if let Some(meta) = codebase.get_class_like(fqcn) {
        return Some(render_class_like(meta));
    }
    if let Some(meta) = codebase.get_function(fqcn) {
        return Some(render_function_like(meta, None));
    }
    if let Some(meta) = codebase.get_constant(fqcn) {
        return Some(format!("```php\nconst {}\n```", BytesDisplay(meta.name.as_bytes())));
    }
    None
}

fn render_class_like(meta: &ClassLikeMetadata) -> String {
    let kind = match meta.kind {
        SymbolKind::Class => "class",
        SymbolKind::Interface => "interface",
        SymbolKind::Trait => "trait",
        SymbolKind::Enum => "enum",
    };

    let mut out = format!("```php\n{kind} {}", BytesDisplay(meta.original_name.as_bytes()));

    if let Some(parent) = meta.direct_parent_class {
        out.push_str(" extends ");
        out.push_str(&String::from_utf8_lossy(parent.as_bytes()));
    }

    if !meta.direct_parent_interfaces.is_empty() {
        let keyword = if matches!(meta.kind, SymbolKind::Interface) { " extends " } else { " implements " };
        out.push_str(keyword);
        let names: Vec<String> =
            meta.direct_parent_interfaces.iter().map(|a| String::from_utf8_lossy(a.as_bytes()).into_owned()).collect();
        out.push_str(&names.join(", "));
    }

    out.push_str("\n```");

    if !meta.used_traits.is_empty() {
        out.push_str("\n\n**Uses traits:** ");
        let names: Vec<String> =
            meta.used_traits.iter().map(|a| String::from_utf8_lossy(a.as_bytes()).into_owned()).collect();
        out.push_str(&names.join(", "));
    }

    out
}

fn render_function_like(meta: &FunctionLikeMetadata, method_of: Option<&[u8]>) -> String {
    let mut signature = String::from("```php\nfunction ");
    if let Some(class) = method_of {
        let _ = write!(signature, "{}", BytesDisplay(class));
        signature.push_str("::");
    }
    let _ = write!(signature, "{}", BytesDisplay(meta.original_name.as_bytes()));
    signature.push('(');
    let mut first = true;
    for param in &meta.parameters {
        if !first {
            signature.push_str(", ");
        }
        first = false;
        if let Some(ty) = &param.type_metadata {
            let _ = write!(signature, "{}", BytesDisplay(ty.type_union.get_id().as_bytes()));
            signature.push(' ');
        }
        let _ = write!(signature, "{}", BytesDisplay(param.name.0.as_bytes()));
    }
    signature.push(')');

    if let Some(rt) = &meta.return_type_metadata {
        signature.push_str(": ");
        let _ = write!(signature, "{}", BytesDisplay(rt.type_union.get_id().as_bytes()));
    }

    signature.push_str("\n```");
    signature
}
