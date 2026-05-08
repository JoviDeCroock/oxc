use oxc_ast::{
    AstKind,
    ast::{AssignmentExpression, AssignmentTarget, BindingPattern, Expression, VariableDeclarator},
};
use oxc_diagnostics::OxcDiagnostic;
use oxc_macros::declare_oxc_lint;
use oxc_span::{GetSpan, Span};
use oxc_str::CompactStr;
use oxc_syntax::operator::AssignmentOperator;
use serde_json::Value;

use crate::{AstNode, context::LintContext, rule::Rule};

fn alias_not_assigned_to_this_diagnostic(span: Span, name: &str) -> OxcDiagnostic {
    OxcDiagnostic::warn(format!("Designated alias '{name}' is not assigned to 'this'."))
        .with_label(span)
}

fn unexpected_alias_diagnostic(span: Span, name: &str) -> OxcDiagnostic {
    OxcDiagnostic::warn(format!("Unexpected alias '{name}' for 'this'.")).with_label(span)
}

#[derive(Debug, Clone)]
struct ConsistentThisConfig {
    aliases: Box<[CompactStr]>,
}

impl Default for ConsistentThisConfig {
    fn default() -> Self {
        Self { aliases: Box::new([CompactStr::from("that")]) }
    }
}

#[derive(Debug, Default, Clone)]
pub struct ConsistentThis(Box<ConsistentThisConfig>);

declare_oxc_lint!(
    /// ### What it does
    ///
    /// Enforces consistent naming when capturing the current execution context with `this`.
    ///
    /// ### Why is this bad?
    ///
    /// It is common practice in JavaScript to capture a reference to the current execution
    /// context (`this`) into a variable so it can be used inside nested functions. Picking a
    /// single, agreed-upon alias (e.g. `that` or `self`) for that capture makes intent clearer
    /// and makes it easier to find these captures across a codebase.
    ///
    /// This rule enforces two things about variables with the designated alias names:
    ///
    /// - if a variable with a designated name is declared and/or assigned, it must be
    ///   initialized (in the case of a declaration) or assigned (in the case of an assignment)
    ///   to the actual current execution context object,
    /// - if a variable is initialized to the current execution context object, it must be
    ///   named with a designated alias.
    ///
    /// ### Examples
    ///
    /// Examples of **incorrect** code for this rule with the default `["that"]` option:
    /// ```js
    /// var that = 42;
    /// var self = this;
    /// that = 42;
    /// self = this;
    /// ```
    ///
    /// Examples of **correct** code for this rule with the default `["that"]` option:
    /// ```js
    /// var that = this;
    /// foo.bar = this;
    /// ```
    ConsistentThis,
    eslint,
    style,
    pending,
    // TODO: Replace this with an actual config struct. This is a dummy value to
    // indicate that this rule has configuration and avoid errors.
    config = Value,
    version = "next",
);

impl Rule for ConsistentThis {
    fn from_configuration(value: Value) -> Result<Self, serde_json::error::Error> {
        let Value::Array(items) = value else {
            return Ok(Self::default());
        };
        let aliases: Box<[CompactStr]> = items
            .iter()
            .filter_map(|item| item.as_str().map(CompactStr::from))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        if aliases.is_empty() {
            Ok(Self::default())
        } else {
            Ok(Self(Box::new(ConsistentThisConfig { aliases })))
        }
    }

    fn run<'a>(&self, node: &AstNode<'a>, ctx: &LintContext<'a>) {
        match node.kind() {
            AstKind::VariableDeclarator(declarator) => {
                self.check_declarator(declarator, ctx);
            }
            AstKind::AssignmentExpression(assignment) => {
                self.check_assignment(assignment, ctx);
            }
            _ => {}
        }
    }
}

impl ConsistentThis {
    fn is_alias(&self, name: &str) -> bool {
        self.0.aliases.iter().any(|alias| alias.as_str() == name)
    }

    fn check_declarator(&self, declarator: &VariableDeclarator<'_>, ctx: &LintContext<'_>) {
        let BindingPattern::BindingIdentifier(ident) = &declarator.id else {
            return;
        };
        let name = ident.name.as_str();

        if let Some(init) = &declarator.init {
            let is_this = matches!(init.without_parentheses(), Expression::ThisExpression(_));
            let is_alias = self.is_alias(name);
            if is_alias && !is_this {
                ctx.diagnostic(alias_not_assigned_to_this_diagnostic(declarator.span, name));
            } else if !is_alias && is_this {
                ctx.diagnostic(unexpected_alias_diagnostic(declarator.span, name));
            }
            return;
        }

        if !self.is_alias(name) {
            return;
        }

        let symbol_id = ident.symbol_id();
        let scoping = ctx.scoping();
        let decl_scope = scoping.symbol_scope_id(symbol_id);

        let has_initialized_declaration = scoping.symbol_declarations(symbol_id).any(|node_id| {
            matches!(
                ctx.nodes().kind(node_id),
                AstKind::VariableDeclarator(decl) if decl.init.is_some()
            )
        });
        if has_initialized_declaration {
            return;
        }

        let assigned_to_this_in_scope =
            scoping.get_resolved_references(symbol_id).any(|reference| {
                if !reference.is_write() || reference.scope_id() != decl_scope {
                    return false;
                }
                let parent = ctx.nodes().parent_kind(reference.node_id());
                let AstKind::AssignmentExpression(assign) = parent else {
                    return false;
                };
                assign.operator == AssignmentOperator::Assign
                    && matches!(assign.right.without_parentheses(), Expression::ThisExpression(_))
            });

        if !assigned_to_this_in_scope {
            ctx.diagnostic(alias_not_assigned_to_this_diagnostic(ident.span, name));
        }
    }

    fn check_assignment(&self, assignment: &AssignmentExpression<'_>, ctx: &LintContext<'_>) {
        let AssignmentTarget::AssignmentTargetIdentifier(target) = &assignment.left else {
            return;
        };
        let name = target.name.as_str();
        let is_this =
            matches!(assignment.right.without_parentheses(), Expression::ThisExpression(_));
        let is_alias = self.is_alias(name);

        if is_alias {
            if !is_this || assignment.operator != AssignmentOperator::Assign {
                ctx.diagnostic(alias_not_assigned_to_this_diagnostic(assignment.span(), name));
            }
        } else if is_this {
            ctx.diagnostic(unexpected_alias_diagnostic(assignment.span(), name));
        }
    }
}

#[test]
fn test() {
    use crate::tester::Tester;

    let pass = vec![
        ("var foo = 42, that = this", None),
        ("var that = (this)", None),
        ("that = (this)", None),
        ("var foo = 42, self = this", Some(serde_json::json!(["self"]))),
        ("var self = 42", Some(serde_json::json!(["that"]))),
        ("var self", Some(serde_json::json!(["that"]))),
        ("var self; self = this", Some(serde_json::json!(["self"]))),
        ("var foo, self; self = this", Some(serde_json::json!(["self"]))),
        ("var foo, self; foo = 42; self = this", Some(serde_json::json!(["self"]))),
        ("self = 42", Some(serde_json::json!(["that"]))),
        ("var foo = {}; foo.bar = this", Some(serde_json::json!(["self"]))),
        ("var self = this; var vm = this;", Some(serde_json::json!(["self", "vm"]))),
        ("var self; var self = this", Some(serde_json::json!(["self"]))),
        ("var self = this; var self", Some(serde_json::json!(["self"]))),
    ];

    let fail = vec![
        ("var context = this", None),
        ("var context = (this)", None),
        ("var that = this", Some(serde_json::json!(["self"]))),
        ("var foo = 42, self = this", Some(serde_json::json!(["that"]))),
        ("var self = 42", Some(serde_json::json!(["self"]))),
        ("var self", Some(serde_json::json!(["self"]))),
        ("var self; self = 42", Some(serde_json::json!(["self"]))),
        ("context = this", Some(serde_json::json!(["that"]))),
        ("that = this", Some(serde_json::json!(["self"]))),
        ("self = this", Some(serde_json::json!(["that"]))),
        ("self += this", Some(serde_json::json!(["self"]))),
        ("var self; var self = 42", Some(serde_json::json!(["self"]))),
        ("var self = 42; var self", Some(serde_json::json!(["self"]))),
        ("var self; (function() { self = this; }())", Some(serde_json::json!(["self"]))),
        ("var self; (function() { self = this; }())", Some(serde_json::json!(["self"]))), // { "ecmaVersion": 6, "sourceType": "module" }
    ];

    Tester::new(ConsistentThis::NAME, ConsistentThis::PLUGIN, pass, fail).test_and_snapshot();
}
