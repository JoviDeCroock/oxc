use oxc_allocator::TakeIn;
use oxc_ast::{AstBuilder, Comment, NONE, ast::*};
use oxc_ast_visit::{Visit, walk::walk_variable_declarator};
use oxc_semantic::ReferenceFlags;
use oxc_span::{GetSpan, SPAN, Span};
use oxc_syntax::{number::NumberBase, scope::ScopeFlags, symbol::SymbolFlags};
use oxc_traverse::{Ancestor, BoundIdentifier, Traverse};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{context::TraverseCtx, state::TransformState};

use super::options::{ReactSignalsMode, ReactSignalsOptions};

const DEFAULT_IMPORT_SOURCE: &str = "@preact/signals-react/runtime";
const USE_SIGNALS_IMPORT: &str = "useSignals";
const MANAGED_COMPONENT: u8 = 1;
const MANAGED_HOOK: u8 = 2;

#[derive(Default)]
struct FunctionMetadata {
    contains_jsx: bool,
    maybe_uses_signal: bool,
}

pub struct ReactSignals<'a> {
    mode: ReactSignalsMode,
    import_source: String,
    detect_transformed_jsx: bool,
    debug: bool,
    no_try_finally: bool,
    function_metadata: FxHashMap<oxc_syntax::scope::ScopeId, FunctionMetadata>,
    opt_in_comments: FxHashSet<u32>,
    opt_out_comments: FxHashSet<u32>,
    jsx_identifiers: FxHashSet<String>,
    jsx_objects: FxHashMap<String, FxHashSet<String>>,
    use_signals_binding: Option<BoundIdentifier<'a>>,
}

impl<'a> ReactSignals<'a> {
    pub fn new(options: &ReactSignalsOptions) -> Self {
        Self {
            mode: options.mode,
            import_source: options
                .import_source
                .clone()
                .unwrap_or_else(|| DEFAULT_IMPORT_SOURCE.to_string()),
            detect_transformed_jsx: options.detect_transformed_jsx,
            debug: options.experimental.debug,
            no_try_finally: options.experimental.no_try_finally,
            function_metadata: FxHashMap::default(),
            opt_in_comments: FxHashSet::default(),
            opt_out_comments: FxHashSet::default(),
            jsx_identifiers: FxHashSet::default(),
            jsx_objects: FxHashMap::default(),
            use_signals_binding: None,
        }
    }

    fn mark_contains_jsx(&mut self, ctx: &TraverseCtx<'a>) {
        let Some(scope_id) = self.find_parent_component_or_hook(ctx) else {
            return;
        };
        self.function_metadata.entry(scope_id).or_default().contains_jsx = true;
    }

    fn mark_maybe_uses_signal(&mut self, ctx: &TraverseCtx<'a>) {
        let Some(scope_id) = self.find_parent_component_or_hook(ctx) else {
            return;
        };
        self.function_metadata.entry(scope_id).or_default().maybe_uses_signal = true;
    }

    fn find_parent_component_or_hook(
        &self,
        ctx: &TraverseCtx<'a>,
    ) -> Option<oxc_syntax::scope::ScopeId> {
        let ancestors: Vec<_> = ctx.ancestors().collect();
        let mut index = 0;

        while index < ancestors.len() {
            let (scope_id, inline_name) = match ancestors[index] {
                Ancestor::FunctionBody(func) => (
                    func.scope_id().get().unwrap(),
                    func.id().as_ref().map(|id| id.name.to_string()),
                ),
                Ancestor::FunctionParams(func) => (
                    func.scope_id().get().unwrap(),
                    func.id().as_ref().map(|id| id.name.to_string()),
                ),
                Ancestor::ArrowFunctionExpressionBody(arrow) => {
                    (arrow.scope_id().get().unwrap(), None)
                }
                Ancestor::ArrowFunctionExpressionParams(arrow) => {
                    (arrow.scope_id().get().unwrap(), None)
                }
                Ancestor::ProgramBody(_)
                | Ancestor::ProgramDirectives(_)
                | Ancestor::ProgramHashbang(_) => break,
                _ => {
                    index += 1;
                    continue;
                }
            };

            let function_name = inline_name
                .or_else(|| self.get_function_name_from_ancestors(index + 1, &ancestors, ctx));

            if function_name.as_deref().is_some_and(is_component_name)
                || function_name.as_deref().is_some_and(is_custom_hook_name)
            {
                return Some(scope_id);
            }

            if self.is_custom_hook_callback(index + 1, &ancestors) {
                return None;
            }

            index += 1;
        }

        None
    }

    fn is_custom_hook_callback(&self, index: usize, ancestors: &[Ancestor<'a, '_>]) -> bool {
        match ancestors.get(index) {
            Some(Ancestor::CallExpressionArguments(call_expr)) => {
                matches!(
                    call_expr.callee().get_inner_expression(),
                    Expression::Identifier(ident) if is_custom_hook_name(&ident.name)
                )
            }
            _ => false,
        }
    }

    fn get_function_name_from_ancestors(
        &self,
        mut index: usize,
        ancestors: &[Ancestor<'a, '_>],
        ctx: &TraverseCtx<'a>,
    ) -> Option<String> {
        while let Some(ancestor) = ancestors.get(index) {
            match ancestor {
                Ancestor::VariableDeclaratorInit(declarator) => {
                    return declarator.id().get_binding_identifier().map(|id| id.name.to_string());
                }
                Ancestor::AssignmentExpressionRight(assignment) => {
                    return assignment_target_name(assignment.left());
                }
                Ancestor::ObjectPropertyValue(property) => {
                    return property.key().static_name().map(|name| name.into_owned());
                }
                Ancestor::ExportDefaultDeclarationDeclaration(_) => {
                    return default_export_name(ctx);
                }
                Ancestor::CallExpressionArguments(_) => {
                    index += 1;
                }
                _ => return None,
            }
        }

        None
    }

    fn should_transform(
        &self,
        scope_id: oxc_syntax::scope::ScopeId,
        function_name: Option<&str>,
        span: Span,
        ctx: &TraverseCtx<'a>,
    ) -> bool {
        if self.is_opted_out(span, ctx) {
            return false;
        }
        if self.is_opted_in(span, ctx) {
            return true;
        }

        let metadata = self.function_metadata.get(&scope_id);
        let contains_jsx = metadata.is_some_and(|metadata| metadata.contains_jsx);
        let maybe_uses_signal = metadata.is_some_and(|metadata| metadata.maybe_uses_signal);
        let is_component = function_name.is_some_and(is_component_name);
        let is_hook = function_name.is_some_and(is_custom_hook_name);

        match self.mode {
            ReactSignalsMode::All => contains_jsx && is_component,
            ReactSignalsMode::Auto => {
                maybe_uses_signal && ((contains_jsx && is_component) || is_hook)
            }
            ReactSignalsMode::Manual => false,
        }
    }

    fn is_opted_in(&self, span: Span, ctx: &TraverseCtx<'a>) -> bool {
        self.has_opt_comment(span, ctx, &self.opt_in_comments)
    }

    fn is_opted_out(&self, span: Span, ctx: &TraverseCtx<'a>) -> bool {
        self.has_opt_comment(span, ctx, &self.opt_out_comments)
    }

    fn has_opt_comment(
        &self,
        span: Span,
        ctx: &TraverseCtx<'a>,
        comments: &FxHashSet<u32>,
    ) -> bool {
        if comments.contains(&span.start) {
            return true;
        }

        for ancestor in ctx.ancestors() {
            match ancestor {
                Ancestor::VariableDeclaratorInit(declarator) => {
                    if comments.contains(&declarator.span().start) {
                        return true;
                    }
                }
                Ancestor::VariableDeclarationDeclarations(declaration) => {
                    if comments.contains(&declaration.span().start) {
                        return true;
                    }
                }
                Ancestor::AssignmentExpressionRight(assignment) => {
                    if comments.contains(&assignment.span().start) {
                        return true;
                    }
                }
                Ancestor::CallExpressionArguments(call_expr) => {
                    if comments.contains(&call_expr.span().start) {
                        return true;
                    }
                }
                Ancestor::ObjectPropertyValue(property) => {
                    return comments.contains(&property.span().start);
                }
                Ancestor::ExportDefaultDeclarationDeclaration(export) => {
                    return comments.contains(&export.span().start);
                }
                Ancestor::ExportNamedDeclarationDeclaration(export) => {
                    return comments.contains(&export.span().start);
                }
                Ancestor::ExpressionStatementExpression(stmt) => {
                    return comments.contains(&stmt.span().start);
                }
                _ => {}
            }
        }

        false
    }

    fn transform_function_body(
        &mut self,
        body: &mut FunctionBody<'a>,
        scope_id: oxc_syntax::scope::ScopeId,
        function_name: Option<&str>,
        ctx: &mut TraverseCtx<'a>,
    ) {
        let use_signals = self.get_use_signals_binding(ctx).create_read_expression(ctx);
        let Some(usage) = self.hook_usage(function_name) else {
            self.prepend_use_signals(body, use_signals, function_name, ctx);
            return;
        };

        let call_arguments = self.create_use_signals_arguments(Some(usage), function_name, ctx);
        let effect_binding =
            ctx.generate_uid("effect", scope_id, SymbolFlags::FunctionScopedVariable);
        let effect_decl = Statement::from(ctx.ast.declaration_variable(
            SPAN,
            VariableDeclarationKind::Var,
            ctx.ast.vec1(ctx.ast.variable_declarator(
                SPAN,
                VariableDeclarationKind::Var,
                effect_binding.create_binding_pattern(ctx),
                NONE,
                Some(ctx.ast.expression_call(SPAN, use_signals, NONE, call_arguments, false)),
                false,
            )),
            false,
        ));

        let try_scope_id = ctx.create_child_scope(scope_id, ScopeFlags::empty());
        let finally_scope_id = ctx.create_child_scope(scope_id, ScopeFlags::empty());

        let directives = body.directives.take_in(ctx.ast);
        let statements = body.statements.take_in(ctx.ast);

        let finally_stmt = ctx.ast.statement_expression(
            SPAN,
            ctx.ast.expression_call(
                SPAN,
                Expression::from(ctx.ast.member_expression_static(
                    SPAN,
                    effect_binding.create_read_expression(ctx),
                    ctx.ast.identifier_name(SPAN, "f"),
                    false,
                )),
                NONE,
                ctx.ast.vec(),
                false,
            ),
        );

        let try_stmt = ctx.ast.statement_try(
            SPAN,
            ctx.ast.block_statement_with_scope_id(SPAN, statements, try_scope_id),
            NONE,
            Some(ctx.ast.alloc_block_statement_with_scope_id(
                SPAN,
                ctx.ast.vec1(finally_stmt),
                finally_scope_id,
            )),
        );

        body.directives = directives;
        body.statements = ctx.ast.vec_from_array([effect_decl, try_stmt]);
    }

    fn prepend_use_signals(
        &mut self,
        body: &mut FunctionBody<'a>,
        use_signals: Expression<'a>,
        function_name: Option<&str>,
        ctx: &mut TraverseCtx<'a>,
    ) {
        let call = ctx.ast.statement_expression(
            SPAN,
            ctx.ast.expression_call(
                SPAN,
                use_signals,
                NONE,
                self.create_use_signals_arguments(None, function_name, ctx),
                false,
            ),
        );
        body.statements.insert(0, call);
    }

    fn create_use_signals_arguments(
        &self,
        usage: Option<u8>,
        function_name: Option<&str>,
        ctx: &mut TraverseCtx<'a>,
    ) -> oxc_allocator::Vec<'a, Argument<'a>> {
        let mut arguments = ctx.ast.vec();

        if let Some(usage) = usage {
            arguments.push(Argument::from(ctx.ast.expression_numeric_literal(
                SPAN,
                f64::from(usage),
                None,
                NumberBase::Decimal,
            )));
            if self.debug
                && let Some(function_name) = function_name
            {
                arguments.push(Argument::from(ctx.ast.expression_string_literal(
                    SPAN,
                    ctx.ast.atom(function_name),
                    None,
                )));
            }
        } else if self.debug
            && let Some(function_name) = function_name
        {
            arguments.push(Argument::from(undefined_expression(ctx)));
            arguments.push(Argument::from(ctx.ast.expression_string_literal(
                SPAN,
                ctx.ast.atom(function_name),
                None,
            )));
        }

        arguments
    }

    fn hook_usage(&self, function_name: Option<&str>) -> Option<u8> {
        if self.no_try_finally {
            return None;
        }
        if function_name.is_some_and(is_custom_hook_name) {
            Some(MANAGED_HOOK)
        } else if function_name.is_some_and(is_component_name) {
            Some(MANAGED_COMPONENT)
        } else {
            None
        }
    }

    fn get_use_signals_binding(&mut self, ctx: &mut TraverseCtx<'a>) -> BoundIdentifier<'a> {
        if let Some(binding) = &self.use_signals_binding {
            return binding.clone();
        }

        let binding = if ctx.state.source_type.is_module() {
            let binding = ctx.generate_uid_in_root_scope(USE_SIGNALS_IMPORT, SymbolFlags::Import);
            ctx.state.module_imports.add_named_import(
                ctx.ast.atom(&self.import_source),
                ctx.ast.atom(USE_SIGNALS_IMPORT),
                binding.clone(),
                false,
            );
            binding
        } else {
            let binding = ctx.generate_uid_in_root_scope(
                USE_SIGNALS_IMPORT,
                SymbolFlags::FunctionScopedVariable,
            );
            let require_symbol_id = ctx.scoping().get_root_binding(ctx.ast.ident("require"));
            let require = ctx.create_ident_expr(
                SPAN,
                ctx.ast.ident("require"),
                require_symbol_id,
                ReferenceFlags::Read,
            );
            let require_call = ctx.ast.expression_call(
                SPAN,
                require,
                NONE,
                ctx.ast.vec1(Argument::from(ctx.ast.expression_string_literal(
                    SPAN,
                    ctx.ast.atom(&self.import_source),
                    None,
                ))),
                false,
            );
            let init = Expression::from(ctx.ast.member_expression_static(
                SPAN,
                require_call,
                ctx.ast.identifier_name(SPAN, USE_SIGNALS_IMPORT),
                false,
            ));
            let stmt = Statement::from(ctx.ast.declaration_variable(
                SPAN,
                VariableDeclarationKind::Var,
                ctx.ast.vec1(ctx.ast.variable_declarator(
                    SPAN,
                    VariableDeclarationKind::Var,
                    binding.create_binding_pattern(ctx),
                    NONE,
                    Some(init),
                    false,
                )),
                false,
            ));
            ctx.state.top_level_statements.insert_statement(stmt);
            binding
        };

        self.use_signals_binding = Some(binding.clone());
        binding
    }

    fn collect_comments(&mut self, comments: &[Comment], source_text: &str) {
        self.opt_in_comments.clear();
        self.opt_out_comments.clear();

        for comment in comments {
            if !comment.is_leading() {
                continue;
            }

            let comment_text = comment.content_span().source_text(source_text);
            if has_comment_directive(comment_text, "@useSignals")
                || has_comment_directive(comment_text, "@trackSignals")
            {
                self.opt_in_comments.insert(comment.attached_to);
            }
            if has_comment_directive(comment_text, "@noUseSignals")
                || has_comment_directive(comment_text, "@noTrackSignals")
            {
                self.opt_out_comments.insert(comment.attached_to);
            }
        }
    }

    fn collect_jsx_alternatives(&mut self, program: &Program<'a>) {
        self.jsx_identifiers.clear();
        self.jsx_objects.clear();

        let mut collector = JSXAlternativeCollector::default();
        collector.visit_program(program);
        self.jsx_identifiers = collector.jsx_identifiers;
        self.jsx_objects = collector.jsx_objects;
    }

    fn is_jsx_alternative_call(&self, call_expr: &CallExpression<'a>) -> bool {
        match &call_expr.callee {
            Expression::Identifier(ident) => self.jsx_identifiers.contains(ident.name.as_str()),
            Expression::StaticMemberExpression(member) => {
                let Expression::Identifier(object) = &member.object else {
                    return false;
                };
                self.jsx_objects
                    .get(object.name.as_str())
                    .is_some_and(|methods| methods.contains(member.property.name.as_str()))
            }
            Expression::ComputedMemberExpression(member) => {
                let Expression::Identifier(object) = &member.object else {
                    return false;
                };
                let Some(property) = member.static_property_name() else {
                    return false;
                };
                self.jsx_objects
                    .get(object.name.as_str())
                    .is_some_and(|methods| methods.contains(property.as_str()))
            }
            _ => false,
        }
    }

    fn maybe_inject_signal_name(
        &self,
        call_expr: &mut CallExpression<'a>,
        ctx: &mut TraverseCtx<'a>,
    ) {
        if !self.debug || !is_signal_call(call_expr) || has_name_in_options(call_expr) {
            return;
        }

        let Some(variable_name) = get_variable_name_from_ancestors(ctx) else {
            return;
        };

        let mut debug_name = variable_name;
        if let Some(filename) = source_basename(ctx)
            && let Some(line) = line_number_for_span(call_expr.span(), ctx.state.source_text)
        {
            debug_name.push_str(" (");
            debug_name.push_str(&filename);
            debug_name.push(':');
            debug_name.push_str(&line.to_string());
            debug_name.push(')');
        }

        let name_property = ctx.ast.object_property_kind_object_property(
            SPAN,
            PropertyKind::Init,
            ctx.ast.property_key_static_identifier(SPAN, "name"),
            ctx.ast.expression_string_literal(SPAN, ctx.ast.atom(&debug_name), None),
            false,
            false,
            false,
        );

        match call_expr.arguments.len() {
            0 => {
                call_expr.arguments.push(Argument::from(undefined_expression(ctx)));
                call_expr.arguments.push(Argument::from(
                    ctx.ast.expression_object(SPAN, ctx.ast.vec1(name_property)),
                ));
            }
            1 => {
                call_expr.arguments.push(Argument::from(
                    ctx.ast.expression_object(SPAN, ctx.ast.vec1(name_property)),
                ));
            }
            _ => match &mut call_expr.arguments[1] {
                Argument::ObjectExpression(object) => {
                    object.properties.push(name_property);
                }
                argument => {
                    *argument = Argument::from(
                        ctx.ast.expression_object(SPAN, ctx.ast.vec1(name_property)),
                    );
                }
            },
        }
    }
}

impl<'a> Traverse<'a, TransformState<'a>> for ReactSignals<'a> {
    fn enter_program(&mut self, program: &mut Program<'a>, _ctx: &mut TraverseCtx<'a>) {
        self.function_metadata.clear();
        self.use_signals_binding = None;
        self.collect_comments(&program.comments, program.source_text);
        if self.detect_transformed_jsx {
            self.collect_jsx_alternatives(program);
        }
    }

    fn enter_call_expression(
        &mut self,
        call_expr: &mut CallExpression<'a>,
        ctx: &mut TraverseCtx<'a>,
    ) {
        if self.detect_transformed_jsx && self.is_jsx_alternative_call(call_expr) {
            self.mark_contains_jsx(ctx);
        }
        self.maybe_inject_signal_name(call_expr, ctx);
    }

    fn enter_member_expression(
        &mut self,
        expr: &mut MemberExpression<'a>,
        ctx: &mut TraverseCtx<'a>,
    ) {
        if expr.static_property_name().is_some_and(|property| property == "value") {
            self.mark_maybe_uses_signal(ctx);
        }
    }

    fn enter_object_pattern(&mut self, pattern: &mut ObjectPattern<'a>, ctx: &mut TraverseCtx<'a>) {
        if pattern
            .properties
            .iter()
            .any(|property| property.key.static_name().is_some_and(|name| name == "value"))
        {
            self.mark_maybe_uses_signal(ctx);
        }
    }

    fn enter_jsx_element(&mut self, _node: &mut JSXElement<'a>, ctx: &mut TraverseCtx<'a>) {
        self.mark_contains_jsx(ctx);
    }

    fn enter_jsx_fragment(&mut self, _node: &mut JSXFragment<'a>, ctx: &mut TraverseCtx<'a>) {
        self.mark_contains_jsx(ctx);
    }

    fn exit_expression(&mut self, expr: &mut Expression<'a>, ctx: &mut TraverseCtx<'a>) {
        match expr {
            Expression::FunctionExpression(func) => {
                let scope_id = func.scope_id();
                let function_name = func.id.as_ref().map(|id| id.name.to_string()).or_else(|| {
                    self.get_function_name_from_ancestors(
                        0,
                        &ctx.ancestors().collect::<Vec<_>>(),
                        ctx,
                    )
                });
                if self.should_transform(scope_id, function_name.as_deref(), func.span, ctx) {
                    self.transform_function_body(
                        func.body.as_mut().unwrap(),
                        scope_id,
                        function_name.as_deref(),
                        ctx,
                    );
                }
            }
            Expression::ArrowFunctionExpression(arrow) => {
                let ancestors = ctx.ancestors().collect::<Vec<_>>();
                let function_name = self.get_function_name_from_ancestors(0, &ancestors, ctx);
                let scope_id = arrow.scope_id();
                let span = arrow.span;
                if self.should_transform(scope_id, function_name.as_deref(), span, ctx) {
                    ensure_arrow_block_body(arrow, ctx.ast);
                    self.transform_function_body(
                        &mut arrow.body,
                        scope_id,
                        function_name.as_deref(),
                        ctx,
                    );
                }
            }
            _ => {}
        }
    }

    fn exit_function(&mut self, func: &mut Function<'a>, ctx: &mut TraverseCtx<'a>) {
        if !func.is_function_declaration() {
            return;
        }

        let function_name = func.id.as_ref().map(|id| id.name.to_string());
        let scope_id = func.scope_id();
        let span = func.span;
        if self.should_transform(scope_id, function_name.as_deref(), span, ctx) {
            self.transform_function_body(
                func.body.as_mut().unwrap(),
                scope_id,
                function_name.as_deref(),
                ctx,
            );
        }
    }
}

#[derive(Default)]
struct JSXAlternativeCollector {
    jsx_identifiers: FxHashSet<String>,
    jsx_objects: FxHashMap<String, FxHashSet<String>>,
}

impl JSXAlternativeCollector {
    fn add_object_methods(&mut self, object: &str, methods: &[&str]) {
        let entry = self.jsx_objects.entry(object.to_string()).or_default();
        entry.extend(methods.iter().map(|method| (*method).to_string()));
    }

    fn jsx_methods(source: &str) -> Option<&'static [&'static str]> {
        match source {
            "react/jsx-runtime" => Some(&["jsx", "jsxs"]),
            "react/jsx-dev-runtime" => Some(&["jsxDEV"]),
            "react" => Some(&["createElement"]),
            _ => None,
        }
    }
}

impl<'a> Visit<'a> for JSXAlternativeCollector {
    fn visit_import_declaration(&mut self, declaration: &ImportDeclaration<'a>) {
        let Some(methods) = Self::jsx_methods(&declaration.source.value) else {
            return;
        };

        for specifier in declaration.specifiers.iter().flatten() {
            match specifier {
                ImportDeclarationSpecifier::ImportSpecifier(specifier) => {
                    if let ModuleExportName::IdentifierName(imported) = &specifier.imported
                        && methods.contains(&imported.name.as_str())
                    {
                        self.jsx_identifiers.insert(specifier.local.name.to_string());
                    }
                }
                ImportDeclarationSpecifier::ImportDefaultSpecifier(specifier) => {
                    self.add_object_methods(&specifier.local.name, methods);
                }
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(specifier) => {
                    self.add_object_methods(&specifier.local.name, methods);
                }
            }
        }
    }

    fn visit_variable_declarator(&mut self, declarator: &VariableDeclarator<'a>) {
        let Some(Expression::CallExpression(call_expr)) = &declarator.init else {
            return;
        };
        let Expression::Identifier(callee) = call_expr.callee.get_inner_expression() else {
            return;
        };
        if callee.name != "require" {
            return;
        }
        let Some(Argument::StringLiteral(source)) = call_expr.arguments.first() else {
            return;
        };
        let Some(methods) = Self::jsx_methods(&source.value) else {
            return;
        };

        match &declarator.id {
            BindingPattern::BindingIdentifier(identifier) => {
                self.add_object_methods(&identifier.name, methods);
            }
            BindingPattern::ObjectPattern(pattern) => {
                for property in &pattern.properties {
                    let Some(imported) = property.key.static_name() else {
                        continue;
                    };
                    if !methods.contains(&imported.as_ref()) {
                        continue;
                    }
                    if let Some(local) = property.value.get_binding_identifier() {
                        self.jsx_identifiers.insert(local.name.to_string());
                    }
                }
            }
            _ => {}
        }

        walk_variable_declarator(self, declarator);
    }

    #[inline]
    fn visit_ts_type_annotation(&mut self, _it: &TSTypeAnnotation<'a>) {}
}

fn has_comment_directive(comment: &str, directive: &str) -> bool {
    comment.split_whitespace().any(|token| token == directive)
}

fn default_export_name(ctx: &TraverseCtx<'_>) -> Option<String> {
    if let Some(name) = ctx.state.source_path.file_name().and_then(|name| name.to_str()) {
        return Some(name.to_string());
    }
    if !ctx.state.filename.is_empty() && ctx.state.filename != "unknown" {
        return Some(ctx.state.filename.clone());
    }
    None
}

fn source_basename(ctx: &TraverseCtx<'_>) -> Option<String> {
    ctx.state.source_path.file_name().and_then(|name| name.to_str()).map(str::to_string).or_else(
        || {
            (!ctx.state.filename.is_empty() && ctx.state.filename != "unknown")
                .then(|| ctx.state.filename.clone())
        },
    )
}

fn line_number_for_span(span: Span, source_text: &str) -> Option<u32> {
    let end = usize::try_from(span.start).ok()?;
    Some(source_text.as_bytes()[..end].iter().filter(|&&b| b == b'\n').count() as u32 + 1)
}

fn undefined_expression<'a>(ctx: &mut TraverseCtx<'a>) -> Expression<'a> {
    let name = ctx.ast.atom("undefined");
    let reference_id = ctx.create_unbound_reference(name.into(), ReferenceFlags::Read);
    ctx.ast.expression_identifier_with_reference_id(SPAN, name, reference_id)
}

fn is_component_name(name: &str) -> bool {
    name.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
}

fn is_custom_hook_name(name: &str) -> bool {
    name.starts_with("use") && name.as_bytes().get(3).is_some_and(u8::is_ascii_uppercase)
}

fn assignment_target_name(target: &AssignmentTarget<'_>) -> Option<String> {
    match target {
        AssignmentTarget::AssignmentTargetIdentifier(ident) => Some(ident.name.to_string()),
        AssignmentTarget::StaticMemberExpression(expr) => Some(expr.property.name.to_string()),
        AssignmentTarget::ComputedMemberExpression(expr) => {
            expr.static_property_name().map(|name| name.to_string())
        }
        _ => None,
    }
}

fn is_signal_call(call_expr: &CallExpression<'_>) -> bool {
    matches!(
        call_expr.callee.get_inner_expression(),
        Expression::Identifier(ident)
            if matches!(ident.name.as_str(), "signal" | "computed" | "useSignal" | "useComputed")
    )
}

fn get_variable_name_from_ancestors(ctx: &TraverseCtx<'_>) -> Option<String> {
    for ancestor in ctx.ancestors() {
        if let Ancestor::VariableDeclaratorInit(declarator) = ancestor {
            return declarator.id().get_binding_identifier().map(|id| id.name.to_string());
        }
    }
    None
}

fn has_name_in_options(call_expr: &CallExpression<'_>) -> bool {
    let Some(Argument::ObjectExpression(object)) = call_expr.arguments.get(1) else {
        return false;
    };

    object.properties.iter().any(|property| {
        matches!(
            property,
            ObjectPropertyKind::ObjectProperty(property)
                if property.key.static_name().is_some_and(|name| name == "name")
        )
    })
}

fn ensure_arrow_block_body<'a>(arrow: &mut ArrowFunctionExpression<'a>, ast: AstBuilder<'a>) {
    if !arrow.expression {
        return;
    }

    arrow.expression = false;
    let Some(Statement::ExpressionStatement(statement)) = arrow.body.statements.pop() else {
        unreachable!("arrow function body is never empty");
    };
    arrow.body.statements.push(ast.statement_return(SPAN, Some(statement.unbox().expression)));
}
