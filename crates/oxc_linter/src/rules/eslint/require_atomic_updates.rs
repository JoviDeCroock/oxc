use oxc_ast::{
    AstKind,
    ast::{
        ArrowFunctionExpression, AssignmentExpression, AssignmentOperator, AssignmentTarget,
        AssignmentTargetMaybeDefault, AssignmentTargetProperty, AwaitExpression,
        ConditionalExpression, Expression, ForInStatement, ForOfStatement, ForStatement,
        ForStatementLeft, FormalParameters, Function, FunctionBody, IdentifierReference,
        IfStatement, LogicalExpression, MemberExpression, Statement, WhileStatement,
        YieldExpression,
    },
};
use oxc_ast_visit::{Visit, walk};
use oxc_diagnostics::OxcDiagnostic;
use oxc_macros::declare_oxc_lint;
use oxc_semantic::{ScopeFlags, ScopeId, Scoping, SymbolId};
use oxc_span::{GetSpan, Span};
use rustc_hash::FxHashSet;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    AstNode,
    context::LintContext,
    rule::{DefaultRuleConfig, Rule},
};

fn require_atomic_updates_diagnostic(span: Span, name: &str) -> OxcDiagnostic {
    OxcDiagnostic::warn(format!(
        "Possible race condition: `{name}` might be reassigned based on an outdated value"
    ))
    .with_help("Avoid reading the variable before an `await`/`yield` and writing to it afterwards.")
    .with_label(span)
}

fn require_atomic_updates_property_diagnostic(span: Span, name: &str) -> OxcDiagnostic {
    OxcDiagnostic::warn(format!(
        "Possible race condition: `{name}` might be assigned based on an outdated state"
    ))
    .with_help("Avoid reading a property and writing to it across an `await`/`yield`.")
    .with_label(span)
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct ConfigElement0 {
    allow_properties: bool,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize, JsonSchema)]
pub struct RequireAtomicUpdates(ConfigElement0);

declare_oxc_lint!(
    /// ### What it does
    ///
    /// Disallows assignments that can lead to race conditions due to usage of `await` or `yield`.
    ///
    /// ### Why is this bad?
    ///
    /// When async code is suspended at an `await` or `yield`, other code can run and
    /// modify shared variables. If the same async function reads a variable, suspends,
    /// and then assigns the variable, the assignment may overwrite changes that
    /// happened during the suspension.
    ///
    /// ### Examples
    ///
    /// Examples of **incorrect** code for this rule:
    /// ```js
    /// let result;
    /// async function foo() {
    ///     result += await something;
    /// }
    ///
    /// async function bar() {
    ///     if (result) {
    ///         result = await update(result);
    ///     }
    /// }
    /// ```
    ///
    /// Examples of **correct** code for this rule:
    /// ```js
    /// let result;
    /// async function foo() {
    ///     result = result + await something;
    /// }
    ///
    /// async function bar() {
    ///     if (result) {
    ///         const tmp = await update(result);
    ///         result = tmp;
    ///     }
    /// }
    /// ```
    RequireAtomicUpdates,
    eslint,
    pedantic,
    pending,
    config = RequireAtomicUpdates,
    version = "next",
);

impl Rule for RequireAtomicUpdates {
    fn from_configuration(value: serde_json::Value) -> Result<Self, serde_json::error::Error> {
        serde_json::from_value::<DefaultRuleConfig<Self>>(value).map(DefaultRuleConfig::into_inner)
    }

    fn run<'a>(&self, node: &AstNode<'a>, ctx: &LintContext<'a>) {
        let allow_properties = self.0.allow_properties;
        match node.kind() {
            AstKind::Function(func) => {
                if !(func.r#async || func.generator) {
                    return;
                }
                let Some(body) = func.body.as_ref() else { return };
                let scope_id = func.scope_id();
                run_analysis(ctx, scope_id, &func.params, body, allow_properties);
            }
            AstKind::ArrowFunctionExpression(arrow) => {
                if !arrow.r#async {
                    return;
                }
                let scope_id = arrow.scope_id();
                run_analysis(ctx, scope_id, &arrow.params, &arrow.body, allow_properties);
            }
            _ => {}
        }
    }
}

fn run_analysis<'a>(
    ctx: &LintContext<'a>,
    func_scope_id: ScopeId,
    params: &FormalParameters<'a>,
    body: &FunctionBody<'a>,
    allow_properties: bool,
) {
    let scoping = ctx.scoping();
    let parameter_symbols = collect_parameter_symbols(params);

    let mut visitor = AtomicVisitor {
        ctx,
        scoping,
        func_scope_id,
        parameter_symbols,
        allow_properties,
        state: State::default(),
    };
    visitor.visit_function_body(body);
}

fn collect_parameter_symbols(params: &FormalParameters<'_>) -> FxHashSet<SymbolId> {
    let mut set = FxHashSet::default();
    for param in &params.items {
        for ident in param.pattern.get_binding_identifiers() {
            if let Some(id) = ident.symbol_id.get() {
                set.insert(id);
            }
        }
    }
    if let Some(rest) = &params.rest {
        for ident in rest.rest.argument.get_binding_identifiers() {
            if let Some(id) = ident.symbol_id.get() {
                set.insert(id);
            }
        }
    }
    set
}

#[derive(Debug, Clone, Default)]
struct State {
    fresh_var: FxHashSet<TrackedValue>,
    outdated_var: FxHashSet<TrackedValue>,
    fresh_prop: FxHashSet<TrackedValue>,
    outdated_prop: FxHashSet<TrackedValue>,
}

impl State {
    fn outdate(&mut self) {
        self.outdated_var.extend(self.fresh_var.drain());
        self.outdated_prop.extend(self.fresh_prop.drain());
    }

    fn mark_var_read(&mut self, key: TrackedValue) {
        self.outdated_var.remove(&key);
        self.fresh_var.insert(key);
    }

    fn mark_prop_read(&mut self, key: TrackedValue) {
        self.outdated_prop.remove(&key);
        self.fresh_prop.insert(key);
    }

    fn merge(&mut self, other: State) {
        self.fresh_var.extend(other.fresh_var);
        self.outdated_var.extend(other.outdated_var);
        self.fresh_prop.extend(other.fresh_prop);
        self.outdated_prop.extend(other.outdated_prop);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum TrackedValue {
    Symbol(SymbolId),
    Global(String),
}

struct AtomicVisitor<'a, 'b> {
    ctx: &'b LintContext<'a>,
    scoping: &'b Scoping,
    func_scope_id: ScopeId,
    parameter_symbols: FxHashSet<SymbolId>,
    allow_properties: bool,
    state: State,
}

impl<'a> AtomicVisitor<'a, '_> {
    fn is_outer(&self, symbol_id: SymbolId) -> bool {
        let symbol_scope = self.scoping.symbol_scope_id(symbol_id);
        if symbol_scope == self.func_scope_id {
            return false;
        }
        let mut current = self.scoping.scope_parent_id(self.func_scope_id);
        while let Some(s) = current {
            if s == symbol_scope {
                return true;
            }
            current = self.scoping.scope_parent_id(s);
        }
        false
    }

    fn has_escape(&self, symbol_id: SymbolId) -> bool {
        for reference in self.scoping.get_resolved_references(symbol_id) {
            if self.is_in_nested_function(reference.scope_id()) {
                return true;
            }
        }
        false
    }

    fn is_in_nested_function(&self, ref_scope: ScopeId) -> bool {
        let mut current = ref_scope;
        loop {
            if current == self.func_scope_id {
                return false;
            }
            let flags = self.scoping.scope_flags(current);
            if flags.is_function() || flags.is_arrow() {
                return true;
            }
            match self.scoping.scope_parent_id(current) {
                Some(p) => current = p,
                None => return true,
            }
        }
    }

    fn is_parameter(&self, symbol_id: SymbolId) -> bool {
        self.parameter_symbols.contains(&symbol_id)
    }

    fn tracked_for_symbol_var(&self, symbol_id: SymbolId) -> bool {
        if self.is_outer(symbol_id) {
            return true;
        }
        if self.is_parameter(symbol_id) {
            return self.has_escape(symbol_id);
        }
        self.has_escape(symbol_id)
    }

    fn tracked_for_symbol_prop(&self, symbol_id: SymbolId) -> bool {
        if self.allow_properties {
            return false;
        }
        if self.is_outer(symbol_id) {
            return true;
        }
        if self.is_parameter(symbol_id) {
            return true;
        }
        self.has_escape(symbol_id)
    }

    fn reference_key(&self, ident: &IdentifierReference<'a>) -> Option<TrackedValue> {
        let reference = self.scoping.get_reference(ident.reference_id());
        if let Some(symbol_id) = reference.symbol_id() {
            return Some(TrackedValue::Symbol(symbol_id));
        }
        self.ctx
            .get_global_variable_value(ident.name.as_str())
            .map(|_| TrackedValue::Global(ident.name.to_string()))
    }

    fn tracked_for_var(&self, key: &TrackedValue) -> bool {
        match key {
            TrackedValue::Symbol(symbol_id) => self.tracked_for_symbol_var(*symbol_id),
            TrackedValue::Global(_) => true,
        }
    }

    fn tracked_for_prop(&self, key: &TrackedValue) -> bool {
        if self.allow_properties {
            return false;
        }
        match key {
            TrackedValue::Symbol(symbol_id) => self.tracked_for_symbol_prop(*symbol_id),
            TrackedValue::Global(_) => true,
        }
    }

    fn key_name<'c>(&'c self, key: &'c TrackedValue) -> &'c str {
        match key {
            TrackedValue::Symbol(symbol_id) => self.scoping.symbol_name(*symbol_id),
            TrackedValue::Global(name) => name.as_str(),
        }
    }

    fn deepest_object_key(&self, expr: &Expression<'a>) -> Option<TrackedValue> {
        match expr {
            Expression::Identifier(ident) => self.reference_key(ident),
            Expression::StaticMemberExpression(m) => self.deepest_object_key(&m.object),
            Expression::ComputedMemberExpression(m) => self.deepest_object_key(&m.object),
            Expression::PrivateFieldExpression(m) => self.deepest_object_key(&m.object),
            Expression::ParenthesizedExpression(p) => self.deepest_object_key(&p.expression),
            Expression::TSAsExpression(e) => self.deepest_object_key(&e.expression),
            Expression::TSSatisfiesExpression(e) => self.deepest_object_key(&e.expression),
            Expression::TSNonNullExpression(e) => self.deepest_object_key(&e.expression),
            Expression::TSTypeAssertion(e) => self.deepest_object_key(&e.expression),
            _ => None,
        }
    }

    fn target_object_key(&self, target: &AssignmentTarget<'a>) -> Option<TrackedValue> {
        match target {
            AssignmentTarget::ComputedMemberExpression(m) => self.deepest_object_key(&m.object),
            AssignmentTarget::StaticMemberExpression(m) => self.deepest_object_key(&m.object),
            AssignmentTarget::PrivateFieldExpression(m) => self.deepest_object_key(&m.object),
            AssignmentTarget::TSAsExpression(e) => self.deepest_target_object(&e.expression),
            AssignmentTarget::TSSatisfiesExpression(e) => self.deepest_target_object(&e.expression),
            AssignmentTarget::TSNonNullExpression(e) => self.deepest_target_object(&e.expression),
            AssignmentTarget::TSTypeAssertion(e) => self.deepest_target_object(&e.expression),
            _ => None,
        }
    }

    fn deepest_target_object(&self, expr: &Expression<'a>) -> Option<TrackedValue> {
        self.deepest_object_key(expr)
    }

    fn snapshot(&self) -> State {
        self.state.clone()
    }

    fn restore(&mut self, snap: State) {
        self.state = snap;
    }

    fn merge(&mut self, snap: State) {
        self.state.merge(snap);
    }

    fn visit_assignment_lhs(&mut self, target: &AssignmentTarget<'a>, is_compound: bool) {
        match target {
            AssignmentTarget::AssignmentTargetIdentifier(ident) => {
                if is_compound {
                    self.visit_identifier_reference(ident);
                }
            }
            AssignmentTarget::ComputedMemberExpression(m) => {
                self.visit_expression(&m.object);
                self.visit_expression(&m.expression);
                if is_compound
                    && let Some(key) = self.deepest_object_key(&m.object)
                    && self.tracked_for_prop(&key)
                {
                    self.state.mark_prop_read(key);
                }
            }
            AssignmentTarget::StaticMemberExpression(m) => {
                self.visit_expression(&m.object);
                if is_compound
                    && let Some(key) = self.deepest_object_key(&m.object)
                    && self.tracked_for_prop(&key)
                {
                    self.state.mark_prop_read(key);
                }
            }
            AssignmentTarget::PrivateFieldExpression(m) => {
                self.visit_expression(&m.object);
                if is_compound
                    && let Some(key) = self.deepest_object_key(&m.object)
                    && self.tracked_for_prop(&key)
                {
                    self.state.mark_prop_read(key);
                }
            }
            // Destructuring patterns and TS wrappers: walk normally.
            _ => walk::walk_assignment_target(self, target),
        }
    }

    fn check_assignment_write(&mut self, target: &AssignmentTarget<'a>) {
        match target {
            AssignmentTarget::AssignmentTargetIdentifier(ident) => {
                let Some(key) = self.reference_key(ident) else { return };
                if self.tracked_for_var(&key) && self.state.outdated_var.contains(&key) {
                    self.ctx.diagnostic(require_atomic_updates_diagnostic(
                        ident.span,
                        ident.name.as_str(),
                    ));
                }
            }
            AssignmentTarget::ArrayAssignmentTarget(target) => {
                for element in target.elements.iter().flatten() {
                    self.check_assignment_target_maybe_default_write(element);
                }
                if let Some(rest) = &target.rest {
                    self.check_assignment_write(&rest.target);
                }
            }
            AssignmentTarget::ObjectAssignmentTarget(target) => {
                for property in &target.properties {
                    match property {
                        AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(prop) => {
                            let Some(key) = self.reference_key(&prop.binding) else { continue };
                            if self.tracked_for_var(&key) && self.state.outdated_var.contains(&key)
                            {
                                self.ctx.diagnostic(require_atomic_updates_diagnostic(
                                    prop.binding.span,
                                    prop.binding.name.as_str(),
                                ));
                            }
                        }
                        AssignmentTargetProperty::AssignmentTargetPropertyProperty(prop) => {
                            self.check_assignment_target_maybe_default_write(&prop.binding);
                        }
                    }
                }
                if let Some(rest) = &target.rest {
                    self.check_assignment_write(&rest.target);
                }
            }
            AssignmentTarget::TSAsExpression(target) => {
                self.check_expression_write(&target.expression);
            }
            AssignmentTarget::TSSatisfiesExpression(target) => {
                self.check_expression_write(&target.expression);
            }
            AssignmentTarget::TSNonNullExpression(target) => {
                self.check_expression_write(&target.expression);
            }
            AssignmentTarget::TSTypeAssertion(target) => {
                self.check_expression_write(&target.expression);
            }
            _ => {
                if self.allow_properties {
                    return;
                }
                if let Some(key) = self.target_object_key(target)
                    && self.tracked_for_prop(&key)
                    && self.state.outdated_prop.contains(&key)
                {
                    let name = self.key_name(&key);
                    self.ctx.diagnostic(require_atomic_updates_property_diagnostic(
                        target.span(),
                        name,
                    ));
                }
            }
        }
    }

    fn check_expression_write(&mut self, expr: &Expression<'a>) {
        match expr {
            Expression::Identifier(ident) => {
                let Some(key) = self.reference_key(ident) else { return };
                if self.tracked_for_var(&key) && self.state.outdated_var.contains(&key) {
                    self.ctx.diagnostic(require_atomic_updates_diagnostic(
                        ident.span,
                        ident.name.as_str(),
                    ));
                }
            }
            Expression::StaticMemberExpression(_)
            | Expression::ComputedMemberExpression(_)
            | Expression::PrivateFieldExpression(_) => {
                if self.allow_properties {
                    return;
                }
                if let Some(key) = self.deepest_object_key(expr)
                    && self.tracked_for_prop(&key)
                    && self.state.outdated_prop.contains(&key)
                {
                    let name = self.key_name(&key);
                    self.ctx
                        .diagnostic(require_atomic_updates_property_diagnostic(expr.span(), name));
                }
            }
            Expression::ParenthesizedExpression(expr) => {
                self.check_expression_write(&expr.expression)
            }
            Expression::TSAsExpression(expr) => self.check_expression_write(&expr.expression),
            Expression::TSSatisfiesExpression(expr) => {
                self.check_expression_write(&expr.expression)
            }
            Expression::TSNonNullExpression(expr) => self.check_expression_write(&expr.expression),
            Expression::TSTypeAssertion(expr) => self.check_expression_write(&expr.expression),
            _ => {}
        }
    }

    fn check_assignment_target_maybe_default_write(
        &mut self,
        target: &AssignmentTargetMaybeDefault<'a>,
    ) {
        match target {
            AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(target) => {
                self.check_assignment_write(&target.binding);
            }
            _ => self.check_assignment_write(target.to_assignment_target()),
        }
    }

    fn check_for_statement_left_write(&mut self, left: &ForStatementLeft<'a>) {
        if let Some(target) = left.as_assignment_target() {
            self.check_assignment_write(target);
        }
    }
}

impl<'a> Visit<'a> for AtomicVisitor<'a, '_> {
    fn visit_function(&mut self, _: &Function<'a>, _: ScopeFlags) {}
    fn visit_arrow_function_expression(&mut self, _: &ArrowFunctionExpression<'a>) {}

    fn visit_identifier_reference(&mut self, ident: &IdentifierReference<'a>) {
        let reference = self.scoping.get_reference(ident.reference_id());
        if !reference.is_read() {
            return;
        }
        let Some(key) = self.reference_key(ident) else { return };
        if self.tracked_for_var(&key) {
            self.state.mark_var_read(key);
        }
    }

    fn visit_member_expression(&mut self, expr: &MemberExpression<'a>) {
        walk::walk_member_expression(self, expr);
        if self.allow_properties {
            return;
        }
        let object = match expr {
            MemberExpression::ComputedMemberExpression(m) => &m.object,
            MemberExpression::StaticMemberExpression(m) => &m.object,
            MemberExpression::PrivateFieldExpression(m) => &m.object,
        };
        if let Some(key) = self.deepest_object_key(object)
            && self.tracked_for_prop(&key)
        {
            self.state.mark_prop_read(key);
        }
    }

    fn visit_await_expression(&mut self, expr: &AwaitExpression<'a>) {
        self.visit_expression(&expr.argument);
        self.state.outdate();
    }

    fn visit_yield_expression(&mut self, expr: &YieldExpression<'a>) {
        if let Some(arg) = &expr.argument {
            self.visit_expression(arg);
        }
        self.state.outdate();
    }

    fn visit_assignment_expression(&mut self, expr: &AssignmentExpression<'a>) {
        let is_compound = expr.operator != AssignmentOperator::Assign;
        self.visit_assignment_lhs(&expr.left, is_compound);
        self.visit_expression(&expr.right);
        self.check_assignment_write(&expr.left);
    }

    fn visit_conditional_expression(&mut self, expr: &ConditionalExpression<'a>) {
        self.visit_expression(&expr.test);
        let snap = self.snapshot();
        self.visit_expression(&expr.consequent);
        let after_cons = self.snapshot();
        self.restore(snap);
        self.visit_expression(&expr.alternate);
        self.merge(after_cons);
    }

    fn visit_logical_expression(&mut self, expr: &LogicalExpression<'a>) {
        self.visit_expression(&expr.left);
        let after_left = self.snapshot();
        self.visit_expression(&expr.right);
        self.merge(after_left);
    }

    fn visit_if_statement(&mut self, stmt: &IfStatement<'a>) {
        self.visit_expression(&stmt.test);
        let snap = self.snapshot();
        self.visit_statement(&stmt.consequent);
        let after_cons = self.snapshot();
        self.restore(snap);
        if let Some(alt) = &stmt.alternate {
            self.visit_statement(alt);
        }
        self.merge(after_cons);
    }

    fn visit_for_statement(&mut self, stmt: &ForStatement<'a>) {
        if let Some(init) = &stmt.init {
            walk::walk_for_statement_init(self, init);
        }
        if let Some(test) = &stmt.test {
            self.visit_expression(test);
        }
        let snap = self.snapshot();
        self.visit_statement(&stmt.body);
        if let Some(update) = &stmt.update {
            self.visit_expression(update);
        }
        self.merge(snap);
    }

    fn visit_while_statement(&mut self, stmt: &WhileStatement<'a>) {
        self.visit_expression(&stmt.test);
        let snap = self.snapshot();
        self.visit_statement(&stmt.body);
        self.merge(snap);
    }

    fn visit_for_in_statement(&mut self, stmt: &ForInStatement<'a>) {
        self.visit_expression(&stmt.right);
        let snap = self.snapshot();
        walk::walk_for_statement_left(self, &stmt.left);
        self.check_for_statement_left_write(&stmt.left);
        self.visit_statement(&stmt.body);
        self.merge(snap);
    }

    fn visit_for_of_statement(&mut self, stmt: &ForOfStatement<'a>) {
        self.visit_expression(&stmt.right);
        let snap = self.snapshot();
        walk::walk_for_statement_left(self, &stmt.left);
        self.check_for_statement_left_write(&stmt.left);
        self.visit_statement(&stmt.body);
        self.merge(snap);
    }

    fn visit_statements(&mut self, stmts: &oxc_allocator::Vec<'a, Statement<'a>>) {
        for stmt in stmts {
            self.visit_statement(stmt);
        }
    }
}

#[test]
fn test() {
    use crate::tester::Tester;

    let pass = vec![
        ("let foo; async function x() { foo += bar; }", None),
        ("let foo; async function x() { foo = foo + bar; }", None),
        ("let foo; async function x() { foo = await bar + foo; }", None),
        ("async function x() { let foo; foo += await bar; }", None),
        ("let foo; async function x() { foo = (await result)(foo); }", None),
        ("let foo; async function x() { foo = bar(await something, foo) }", None),
        ("function* x() { let foo; foo += yield bar; }", None),
        ("const foo = {}; async function x() { foo.bar = await baz; }", None),
        ("const foo = []; async function x() { foo[x] += 1;  }", None),
        ("let foo; function* x() { foo = bar + foo; }", None),
        ("async function x() { let foo; bar(() => baz += 1); foo += await amount; }", None),
        ("let foo; async function x() { foo = condition ? foo : await bar; }", None),
        (
            "async function x() { let foo; bar(() => { let foo; blah(foo); }); foo += await result; }",
            None,
        ),
        ("let foo; async function x() { foo = foo + 1; await bar; }", None),
        ("async function x() { foo += await bar; }", None),
        (
            "
                        async function f() {
                            let records
                            records = await a.records
                            g(() => { records })
                        }
                    ",
            None,
        ),
        (
            "
                        async function f() {
                            try {
                                this.foo = doSomething();
                            } catch (e) {
                                this.foo = null;
                                await doElse();
                            }
                        }
                    ",
            None,
        ),
        (
            "
                        async function f(foo) {
                            let bar = await get(foo.id);
                            bar.prop = foo.prop;
                        }
                    ",
            None,
        ),
        (
            "
                        async function f(foo) {
                            let bar = await get(foo.id);
                            foo = bar.prop;
                        }
                    ",
            None,
        ),
        (
            "
                        async function f() {
                            let foo = {}
                            let bar = await get(foo.id);
                            foo.prop = bar.prop;
                        }
                    ",
            None,
        ),
        (
            "
                        let count = 0
                        let queue = []
                        async function A(...args) {
                            count += 1
                            await new Promise(resolve=>resolve())
                            count -= 1
                            return
                        }
                    ",
            None,
        ),
        (
            "
                        async function run() {
                          {
                            let entry;
                            await entry;
                          }
                          {
                            let entry;
                            () => entry;

                            entry = 1;
                          }
                        }
                    ",
            None,
        ),
        (
            "
                        async function run() {
                            await a;
                            b = 1;
                        }
                    ",
            None,
        ),
        (
            "
                            async function a(foo) {
                                if (foo.bar) {
                                    foo.bar = await something;
                                }
                            }
                        ",
            Some(serde_json::json!([{ "allowProperties": true }])),
        ),
        (
            "
                            function* g(foo) {
                                baz = foo.bar;
                                yield something;
                                foo.bar = 1;
                            }
                        ",
            Some(serde_json::json!([{ "allowProperties": true }])),
        ),
    ];

    let fail = vec![
        ("let foo; async function x() { foo += await amount; }", None),
        ("if (1); let foo; async function x() { foo += await amount; }", None),
        ("let foo; async function x() { while (condition) { foo += await amount; } }", None),
        ("let foo; async function x() { foo = foo + await amount; }", None),
        ("let foo; async function x() { foo = foo + (bar ? baz : await amount); }", None),
        ("let foo; async function x() { foo = foo + (bar ? await amount : baz); }", None),
        (
            "let foo; async function x() { foo = condition ? foo + await amount : somethingElse; }",
            None,
        ),
        ("let foo; async function x() { foo = (condition ? foo : await bar) + await bar; }", None),
        ("let foo; async function x() { foo += bar + await amount; }", None),
        ("async function x() { let foo; bar(() => foo); foo += await amount; }", None),
        ("let foo; function* x() { foo += yield baz }", None),
        ("let foo; async function x() { foo = bar(foo, await something) }", None),
        ("const foo = {}; async function x() { foo.bar += await baz }", None),
        ("const foo = []; async function x() { foo[bar].baz += await result;  }", None),
        ("const foo = {}; class C { #bar; async wrap() { foo.#bar += await baz } }", None),
        ("let foo; async function* x() { foo = (yield foo) + await bar; }", None),
        ("let foo; async function x() { foo = foo + await result(foo); }", None),
        ("let foo; async function x() { foo = await result(foo, await somethingElse); }", None),
        ("function* x() { let foo; yield async function y() { foo += await bar; } }", None),
        ("let foo; async function* x() { foo = await foo + (yield bar); }", None),
        ("let foo; async function x() { foo = bar + await foo; }", None),
        (
            "let foo = {}; async function x() { foo[bar].baz = await (foo.bar += await foo[bar].baz) }",
            None,
        ),
        ("let foo = ''; async function x() { foo += await bar; }", None),
        ("let foo = 0; async function x() { foo = (a ? b : foo) + await bar; if (baz); }", None),
        (
            "let foo = 0; async function x() { foo = (a ? b ? c ? d ? foo : e : f : g : h) + await bar; if (baz); }",
            None,
        ),
        (
            "
                            async function f(foo) {
                                let buz = await get(foo.id);
                                foo.bar = buz.bar;
                            }
                        ",
            None,
        ),
        (
            "
                            async function a(foo) {
                                if (foo.bar) {
                                    foo.bar = await something;
                                }
                            }
                        ",
            None,
        ),
        (
            "
                            function* g(foo) {
                                baz = foo.bar;
                                yield something;
                                foo.bar = 1;
                            }
                        ",
            None,
        ),
        (
            "
                            async function a(foo) {
                                if (foo.bar) {
                                    foo.bar = await something;
                                }
                            }
                        ",
            Some(serde_json::json!([{}])),
        ),
        (
            "
                            function* g(foo) {
                                baz = foo.bar;
                                yield something;
                                foo.bar = 1;
                            }
                        ",
            Some(serde_json::json!([{}])),
        ),
        (
            "
                            async function a(foo) {
                                if (foo.bar) {
                                    foo.bar = await something;
                                }
                            }
                        ",
            Some(serde_json::json!([{ "allowProperties": false }])),
        ),
        (
            "
                            function* g(foo) {
                                baz = foo.bar;
                                yield something;
                                foo.bar = 1;
                            }
                        ",
            Some(serde_json::json!([{ "allowProperties": false }])),
        ),
        (
            "
                            let foo;
                            async function a() {
                                if (foo) {
                                    foo = await something;
                                }
                            }
                        ",
            Some(serde_json::json!([{ "allowProperties": true }])),
        ),
        (
            "
                            let foo;
                            function* g() {
                                baz = foo;
                                yield something;
                                foo = 1;
                            }
            ",
            Some(serde_json::json!([{ "allowProperties": true }])),
        ),
        ("async function x() { globalThis.foo; await bar; globalThis.foo = 1; }", None),
        ("let foo; async function x() { if (foo) await bar; ({foo} = obj); }", None),
        ("let foo; async function x() { if (foo) await bar; for (foo of xs) {} }", None),
        ("let foo; async function x() { if (foo) { foo = await bar; foo = 1; } }", None),
    ];

    Tester::new(RequireAtomicUpdates::NAME, RequireAtomicUpdates::PLUGIN, pass, fail)
        .test_and_snapshot();
}
