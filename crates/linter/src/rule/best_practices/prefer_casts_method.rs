use indoc::indoc;
use mago_allocator::Arena;
use mago_text_edit::Safety;
use mago_text_edit::TextEdit;
use schemars::JsonSchema;

use mago_reporting::Annotation;
use mago_reporting::Issue;
use mago_reporting::Level;
use mago_span::HasSpan;
use mago_syntax::cst::ClassLikeMember;
use mago_syntax::cst::Expression;
use mago_syntax::cst::Modifier;
use mago_syntax::cst::Node;
use mago_syntax::cst::NodeKind;
use mago_syntax::cst::Property;
use mago_syntax::cst::PropertyItem;

use crate::category::Category;
use crate::context::LintContext;
use crate::integration::Integration;
use crate::requirements::RuleRequirements;
use crate::rule::Config;
use crate::rule::LintRule;
use crate::rule_meta::RuleMeta;
use crate::settings::RuleSettings;

#[derive(Debug, Clone)]
pub struct PreferCastsMethodRule {
    meta: &'static RuleMeta,
    cfg: PreferCastsMethodConfig,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, JsonSchema)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, rename_all = "kebab-case", deny_unknown_fields))]
pub struct PreferCastsMethodConfig {
    pub level: Level,
}

impl Default for PreferCastsMethodConfig {
    fn default() -> Self {
        Self { level: Level::Note }
    }
}

impl Config for PreferCastsMethodConfig {
    fn level(&self) -> Level {
        self.level
    }
}

impl LintRule for PreferCastsMethodRule {
    type Config = PreferCastsMethodConfig;

    fn meta() -> &'static RuleMeta {
        const META: RuleMeta = RuleMeta {
            name: "Prefer Casts Method",
            code: "prefer-casts-method",
            description: indoc! {"
                Detects the `$casts` property on an Eloquent model. Laravel 11 introduced the `casts()`
                method as the modern way to declare attribute casts, allowing the cast values to be
                expressed with code (such as enum or class references) rather than a static array.
            "},
            good_example: indoc! {r"
                <?php

                class User
                {
                    protected function casts(): array
                    {
                        return [
                            'is_admin' => 'boolean',
                        ];
                    }
                }
            "},
            bad_example: indoc! {r"
                <?php

                class User
                {
                    protected $casts = [
                        'is_admin' => 'boolean',
                    ];
                }
            "},
            category: Category::BestPractices,
            requirements: RuleRequirements::Integration(Integration::Laravel),
        };

        &META
    }

    fn targets() -> &'static [NodeKind] {
        const TARGETS: &[NodeKind] = &[NodeKind::Property];

        TARGETS
    }

    fn build(settings: &RuleSettings<Self::Config>) -> Self {
        Self { meta: Self::meta(), cfg: settings.config }
    }

    fn check<'arena, A>(&self, ctx: &mut LintContext<'_, 'arena, A>, node: Node<'_, 'arena>)
    where
        A: Arena,
    {
        let Node::Property(Property::Plain(property)) = node else {
            return;
        };

        // A `casts()` method is never static, so a static `$casts` is left alone.
        if property.modifiers.iter().any(|modifier| matches!(modifier, Modifier::Static(_))) {
            return;
        }

        for item in property.items.iter() {
            let PropertyItem::Concrete(concrete) = item else {
                continue;
            };

            // Property names are case-sensitive in PHP.
            if concrete.variable.name != b"$casts" {
                continue;
            }

            if !matches!(concrete.value, Expression::Array(_) | Expression::LegacyArray(_)) {
                continue;
            }

            let issue = Issue::new(
                self.cfg.level(),
                "Define model casts in the `casts()` method instead of the `$casts` property.",
            )
            .with_code(self.meta.code)
            .with_annotation(
                Annotation::primary(concrete.variable.span())
                    .with_message("This `$casts` property should be a `casts()` method"),
            )
            .with_note("Laravel 11 introduced the `casts()` method as the modern way to declare attribute casts.")
            .with_help(
                "Move the cast definitions into a `protected function casts(): array` method that returns the array.",
            );

            // We can only rewrite `$casts` into a `casts()` method when the property declaration is
            // self-contained: a single `$casts` item, with no attributes to relocate, and no existing
            // `casts()` method that the new one would clash with.
            let is_sole_item = property.items.iter().count() == 1;
            if !is_sole_item || !property.attribute_lists.is_empty() || enclosing_has_casts_method(ctx) {
                ctx.collector.report(issue);

                return;
            }

            let source = ctx.source_file.contents.as_ref();
            let property_span = property.span();
            let start = property_span.start.offset;
            let end = property_span.end.offset;

            let indent = line_indent(source, start as usize);
            let visibility = property
                .modifiers
                .iter()
                .find_map(|modifier| match modifier {
                    Modifier::Public(_) => Some("public"),
                    Modifier::Protected(_) => Some("protected"),
                    Modifier::Private(_) => Some("private"),
                    _ => None,
                })
                .unwrap_or("protected");

            let value_span = concrete.value.span();
            let array = reindent(&String::from_utf8_lossy(
                &source[value_span.start.offset as usize..value_span.end.offset as usize],
            ));

            let method =
                format!("{visibility} function casts(): array\n{indent}{{\n{indent}    return {array};\n{indent}}}",);

            ctx.collector.propose(issue, move |edits| {
                // Relies on the array being reindented one level deeper; keep manual review in the loop.
                edits.push(TextEdit::replace(start..end, method).with_safety(Safety::PotentiallyUnsafe));
            });

            return;
        }
    }
}

/// The leading whitespace of the line that `offset` falls on.
fn line_indent(source: &[u8], offset: usize) -> String {
    let line_start = source[..offset].iter().rposition(|&byte| byte == b'\n').map_or(0, |index| index + 1);

    source[line_start..offset]
        .iter()
        .take_while(|&&byte| byte == b' ' || byte == b'\t')
        .map(|&byte| byte as char)
        .collect()
}

/// Adds one indentation level (four spaces) to every line after the first, so an array lifted out of
/// a property initializer sits correctly inside the `return` of the generated `casts()` method: the
/// initializer nests one level deeper as a method body, so each continuation line shifts by one unit.
fn reindent(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            result.push('\n');
            result.push_str("    ");
        }

        result.push_str(line);
    }

    result
}

/// Whether the class-like that encloses the current property already declares a `casts` method
/// (method names are case-insensitive in PHP), in which case generating another would be a conflict.
fn enclosing_has_casts_method<A>(ctx: &LintContext<'_, '_, A>) -> bool
where
    A: Arena,
{
    for depth in 0..4 {
        let Some(ancestor) = ctx.get_nth_parent(depth) else {
            break;
        };

        let members = match ancestor {
            Node::Class(class) => &class.members,
            Node::AnonymousClass(class) => &class.members,
            Node::Trait(r#trait) => &r#trait.members,
            Node::Enum(r#enum) => &r#enum.members,
            Node::Interface(interface) => &interface.members,
            _ => continue,
        };

        return members.iter().any(|member| {
            matches!(member, ClassLikeMember::Method(method) if method.name.value.eq_ignore_ascii_case(b"casts"))
        });
    }

    false
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;
    use crate::test_lint_failure;
    use crate::test_lint_fix;
    use crate::test_lint_success;

    test_lint_failure! {
        name = casts_property,
        rule = PreferCastsMethodRule,
        code = indoc! {r"
            <?php

            class User
            {
                protected $casts = [
                    'options' => 'array',
                ];
            }
        "}
    }

    test_lint_fix! {
        name = fix_multiline_casts_property,
        rule = PreferCastsMethodRule,
        code = indoc! {r"
            <?php

            class User
            {
                protected $casts = [
                    'options' => 'array',
                ];
            }
        "},
        fixed = indoc! {r"
            <?php

            class User
            {
                protected function casts(): array
                {
                    return [
                        'options' => 'array',
                    ];
                }
            }
        "}
    }

    test_lint_fix! {
        name = fix_empty_casts_property,
        rule = PreferCastsMethodRule,
        code = indoc! {r"
            <?php

            class User
            {
                protected $casts = [];
            }
        "},
        fixed = indoc! {r"
            <?php

            class User
            {
                protected function casts(): array
                {
                    return [];
                }
            }
        "}
    }

    test_lint_failure! {
        name = casts_property_with_existing_method_is_not_fixed,
        rule = PreferCastsMethodRule,
        code = indoc! {r"
            <?php

            class User
            {
                protected $casts = [
                    'options' => 'array',
                ];

                protected function casts(): array
                {
                    return [];
                }
            }
        "}
    }

    test_lint_success! {
        name = casts_method,
        rule = PreferCastsMethodRule,
        code = indoc! {r"
            <?php

            class User
            {
                protected function casts(): array
                {
                    return ['options' => 'array'];
                }
            }
        "}
    }

    test_lint_success! {
        name = other_property,
        rule = PreferCastsMethodRule,
        code = indoc! {r"
            <?php

            class User
            {
                protected $fillable = ['name', 'email'];
            }
        "}
    }

    test_lint_success! {
        name = casts_without_array_value,
        rule = PreferCastsMethodRule,
        code = indoc! {r"
            <?php

            class User
            {
                protected $casts;
            }
        "}
    }

    test_lint_success! {
        name = static_casts_left_alone,
        rule = PreferCastsMethodRule,
        code = indoc! {r"
            <?php

            class Registry
            {
                protected static $casts = ['a' => 'b'];
            }
        "}
    }
}
