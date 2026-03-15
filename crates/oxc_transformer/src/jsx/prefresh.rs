use std::{collections::hash_map::Entry, iter, str};

use base64::{
    encoded_len as base64_encoded_len,
    prelude::{BASE64_STANDARD, Engine},
};
use rustc_hash::{FxHashMap, FxHashSet};
use sha1::{Digest, Sha1};

use oxc_allocator::{
    CloneIn, GetAddress, StringBuilder as ArenaStringBuilder, TakeIn, UnstableAddress,
    Vec as ArenaVec,
};
use oxc_ast::{AstBuilder, NONE, ast::*, match_expression};
use oxc_ast_visit::{
    Visit,
    walk::{walk_call_expression, walk_declaration},
};
use oxc_semantic::{ReferenceFlags, ScopeFlags, ScopeId, SymbolFlags, SymbolId};
use oxc_span::{Atom, GetSpan, Ident, SPAN};
use oxc_syntax::operator::{AssignmentOperator, LogicalOperator};
use oxc_traverse::{Ancestor, BoundIdentifier, Traverse};

use crate::{
    common::var_declarations::VarDeclarationsStore, context::TraverseCtx, state::TransformState,
};

use super::options::PrefreshOptions;

#[derive(Debug)]
enum RefreshIdentifierResolver<'a> {
    Identifier(IdentifierReference<'a>),
    Member((IdentifierReference<'a>, IdentifierName<'a>)),
    Expression(Expression<'a>),
}

impl<'a> RefreshIdentifierResolver<'a> {
    fn parse(input: &str, ast: AstBuilder<'a>) -> Self {
        let mut parts = input.split('.');

        let first_part = parts.next().unwrap();
        let Some(second_part) = parts.next() else {
            return Self::Identifier(ast.identifier_reference(SPAN, ast.atom(input)));
        };

        if first_part == "import" {
            let mut expr = ast.expression_meta_property(
                SPAN,
                ast.identifier_name(SPAN, "import"),
                ast.identifier_name(SPAN, ast.atom(second_part)),
            );
            if let Some(property) = parts.next() {
                expr = Expression::from(ast.member_expression_static(
                    SPAN,
                    expr,
                    ast.identifier_name(SPAN, ast.atom(property)),
                    false,
                ));
            }
            return Self::Expression(expr);
        }

        let object = ast.identifier_reference(SPAN, ast.atom(first_part));
        let property = ast.identifier_name(SPAN, ast.atom(second_part));
        Self::Member((object, property))
    }

    fn to_expression(&self, ctx: &mut TraverseCtx<'a>) -> Expression<'a> {
        match self {
            Self::Identifier(ident) => {
                let reference_id = ctx.create_unbound_reference(ident.name, ReferenceFlags::Read);
                ctx.ast.expression_identifier_with_reference_id(
                    ident.span,
                    ident.name,
                    reference_id,
                )
            }
            Self::Member((ident, property)) => {
                let reference_id = ctx.create_unbound_reference(ident.name, ReferenceFlags::Read);
                let ident = ctx.ast.expression_identifier_with_reference_id(
                    ident.span,
                    ident.name,
                    reference_id,
                );
                Expression::from(ctx.ast.member_expression_static(
                    SPAN,
                    ident,
                    property.clone(),
                    false,
                ))
            }
            Self::Expression(expr) => expr.clone_in(ctx.ast.allocator),
        }
    }
}

pub struct Prefresh<'a> {
    refresh_reg: RefreshIdentifierResolver<'a>,
    refresh_sig: RefreshIdentifierResolver<'a>,
    emit_full_signatures: bool,
    registrations: Vec<(BoundIdentifier<'a>, Atom<'a>)>,
    last_signature: Option<(BindingIdentifier<'a>, ArenaVec<'a, Argument<'a>>)>,
    function_signature_keys: FxHashMap<ScopeId, String>,
    non_builtin_hooks_callee: FxHashMap<ScopeId, Vec<Option<Expression<'a>>>>,
    used_in_jsx_bindings: FxHashSet<SymbolId>,
    create_context_bindings: FxHashSet<SymbolId>,
    file_hash: String,
    contexts: FxHashMap<String, u32>,
}

impl<'a> Prefresh<'a> {
    pub fn new(options: &PrefreshOptions, ast: AstBuilder<'a>) -> Self {
        Self {
            refresh_reg: RefreshIdentifierResolver::parse(&options.refresh_reg, ast),
            refresh_sig: RefreshIdentifierResolver::parse(&options.refresh_sig, ast),
            emit_full_signatures: options.emit_full_signatures,
            registrations: Vec::default(),
            last_signature: None,
            function_signature_keys: FxHashMap::default(),
            non_builtin_hooks_callee: FxHashMap::default(),
            used_in_jsx_bindings: FxHashSet::default(),
            create_context_bindings: FxHashSet::default(),
            file_hash: String::new(),
            contexts: FxHashMap::default(),
        }
    }
}

impl<'a> Traverse<'a, TransformState<'a>> for Prefresh<'a> {
    fn enter_program(&mut self, program: &mut Program<'a>, ctx: &mut TraverseCtx<'a>) {
        self.used_in_jsx_bindings = UsedInJSXBindingsCollector::collect(program, ctx);
        self.collect_create_context_bindings(program);
        self.file_hash = hash_base36(&Self::context_filename(ctx));
        self.contexts.clear();

        let mut new_statements = ctx.ast.vec_with_capacity(program.body.len() * 2);
        for mut statement in program.body.take_in(ctx.ast) {
            let next_statement = self.process_statement(&mut statement, ctx);
            new_statements.push(statement);
            if let Some(assignment_expression) = next_statement {
                new_statements.push(assignment_expression);
            }
        }
        program.body = new_statements;
    }

    fn exit_program(&mut self, program: &mut Program<'a>, ctx: &mut TraverseCtx<'a>) {
        if self.registrations.is_empty() {
            return;
        }

        let var_decl = Statement::from(ctx.ast.declaration_variable(
            SPAN,
            VariableDeclarationKind::Var,
            ctx.ast.vec(),
            false,
        ));

        let mut variable_declarator_items = ctx.ast.vec_with_capacity(self.registrations.len());
        let calls = self.registrations.iter().map(|(binding, persistent_id)| {
            variable_declarator_items.push(ctx.ast.variable_declarator(
                SPAN,
                VariableDeclarationKind::Var,
                binding.create_binding_pattern(ctx),
                NONE,
                None,
                false,
            ));

            let callee = self.refresh_reg.to_expression(ctx);
            let arguments = ctx.ast.vec_from_array([
                Argument::from(binding.create_read_expression(ctx)),
                Argument::from(ctx.ast.expression_string_literal(SPAN, *persistent_id, None)),
            ]);
            ctx.ast.statement_expression(
                SPAN,
                ctx.ast.expression_call(SPAN, callee, NONE, arguments, false),
            )
        });

        let var_decl_index = program.body.len();
        program.body.extend(iter::once(var_decl).chain(calls));

        let Statement::VariableDeclaration(var_decl) = &mut program.body[var_decl_index] else {
            unreachable!()
        };
        var_decl.declarations = variable_declarator_items;
    }

    fn enter_call_expression(
        &mut self,
        call_expr: &mut CallExpression<'a>,
        ctx: &mut TraverseCtx<'a>,
    ) {
        let current_scope_id = ctx.current_scope_id();
        if !ctx.scoping().scope_flags(current_scope_id).is_function() {
            return;
        }

        let hook_name: Atom = match &call_expr.callee {
            Expression::Identifier(ident) => ident.name.into(),
            Expression::StaticMemberExpression(member) => member.property.name.into(),
            _ => return,
        };

        if !is_use_hook_name(&hook_name) {
            return;
        }

        if !is_builtin_hook(&hook_name) {
            let (binding_name, is_member_expression): (Option<Ident>, _) = match &call_expr.callee {
                Expression::Identifier(ident) => (Some(ident.name), false),
                Expression::StaticMemberExpression(member) => {
                    if let Expression::Identifier(object) = &member.object {
                        (Some(object.name), true)
                    } else {
                        (None, false)
                    }
                }
                _ => unreachable!(),
            };

            if let Some(binding_name) = binding_name {
                self.non_builtin_hooks_callee.entry(current_scope_id).or_default().push(
                    ctx.scoping()
                        .find_binding(
                            ctx.scoping().scope_parent_id(ctx.current_scope_id()).unwrap(),
                            binding_name,
                        )
                        .map(|symbol_id| {
                            let mut expr = ctx.create_bound_ident_expr(
                                SPAN,
                                binding_name,
                                symbol_id,
                                ReferenceFlags::Read,
                            );

                            if is_member_expression {
                                expr = Expression::from(ctx.ast.member_expression_static(
                                    SPAN,
                                    expr,
                                    ctx.ast.identifier_name(SPAN, hook_name),
                                    false,
                                ));
                            }
                            expr
                        }),
                );
            }
        }

        let declarator_id = if let Ancestor::VariableDeclaratorInit(declarator) = ctx.parent() {
            declarator.id().span().source_text(ctx.state.source_text)
        } else {
            ""
        };

        let args = &call_expr.arguments;
        let (args_key, mut key_len) = if hook_name == "useState" && !args.is_empty() {
            let args_key = args[0].span().source_text(ctx.state.source_text);
            (args_key, args_key.len() + 4)
        } else if hook_name == "useReducer" && args.len() > 1 {
            let args_key = args[1].span().source_text(ctx.state.source_text);
            (args_key, args_key.len() + 4)
        } else if hook_name == "useSignal" && !args.is_empty() {
            let args_key = args[0].span().source_text(ctx.state.source_text);
            (args_key, args_key.len() + 4)
        } else {
            ("", 2)
        };

        key_len += hook_name.len() + declarator_id.len();

        let string = match self.function_signature_keys.entry(current_scope_id) {
            Entry::Occupied(entry) => {
                let string = entry.into_mut();
                string.reserve(key_len + 2);
                string.push_str("\\n");
                string
            }
            Entry::Vacant(entry) => entry.insert(String::with_capacity(key_len)),
        };

        string.push_str(&hook_name);
        string.push('{');
        string.push_str(declarator_id);
        if !args_key.is_empty() {
            string.push('(');
            string.push_str(args_key);
            string.push(')');
        }
        string.push('}');
    }

    fn exit_expression(&mut self, expr: &mut Expression<'a>, ctx: &mut TraverseCtx<'a>) {
        if let Expression::CallExpression(call_expr) = expr
            && self.is_create_context_call(call_expr, ctx)
        {
            *expr = self.transform_create_context_call(call_expr, ctx);
            return;
        }

        let signature = match expr {
            Expression::FunctionExpression(func) => self.create_signature_call_expression(
                func.scope_id(),
                func.body.as_mut().unwrap(),
                ctx,
            ),
            Expression::ArrowFunctionExpression(arrow) => {
                let call_fn =
                    self.create_signature_call_expression(arrow.scope_id(), &mut arrow.body, ctx);

                if call_fn.is_some() {
                    Self::transform_arrow_function_to_block(arrow, ctx);
                }
                call_fn
            }
            Expression::AssignmentExpression(_) => return,
            Expression::CallExpression(_) => self.last_signature.take(),
            _ => None,
        };

        let Some((binding_identifier, mut arguments)) = signature else {
            return;
        };
        let binding = BoundIdentifier::from_binding_ident(&binding_identifier);

        if !matches!(expr, Expression::CallExpression(_)) {
            if let Ancestor::VariableDeclaratorInit(declarator) = ctx.parent()
                && let Some(ident) = declarator.id().get_binding_identifier()
            {
                let id_binding = BoundIdentifier::from_binding_ident(ident);
                self.handle_function_in_variable_declarator(&id_binding, &binding, arguments, ctx);
                return;
            }
        }

        let mut found_call_expression = false;
        for ancestor in ctx.ancestors() {
            if ancestor.is_assignment_expression() {
                continue;
            }
            if ancestor.is_call_expression() {
                found_call_expression = true;
            }
            break;
        }

        if found_call_expression {
            self.last_signature =
                Some((binding_identifier.clone(), arguments.clone_in(ctx.ast.allocator)));
        }

        let span = expr.span();
        arguments.insert(0, Argument::from(expr.take_in(ctx.ast)));
        *expr = ctx.ast.expression_call(
            span,
            binding.create_read_expression(ctx),
            NONE,
            arguments,
            false,
        );
    }

    fn exit_function(&mut self, func: &mut Function<'a>, ctx: &mut TraverseCtx<'a>) {
        if !func.is_function_declaration() {
            return;
        }

        let Some((binding_identifier, mut arguments)) = self.create_signature_call_expression(
            func.scope_id(),
            func.body.as_mut().unwrap(),
            ctx,
        ) else {
            return;
        };

        let Some(id) = func.id.as_ref() else {
            return;
        };
        let id_binding = BoundIdentifier::from_binding_ident(id);

        arguments.insert(0, Argument::from(id_binding.create_read_expression(ctx)));

        let binding = BoundIdentifier::from_binding_ident(&binding_identifier);
        let callee = binding.create_read_expression(ctx);
        let expr = ctx.ast.expression_call(func.span, callee, NONE, arguments, false);
        let statement = ctx.ast.statement_expression(func.span, expr);

        let address = match ctx.parent() {
            Ancestor::ExportNamedDeclarationDeclaration(decl) => decl.address(),
            Ancestor::ExportDefaultDeclarationDeclaration(decl) => decl.address(),
            _ => func.unstable_address(),
        };
        ctx.state.statement_injector.insert_after(&address, statement);
    }
}

impl<'a> Prefresh<'a> {
    fn create_registration(
        &mut self,
        persistent_id: Atom<'a>,
        ctx: &mut TraverseCtx<'a>,
    ) -> AssignmentTarget<'a> {
        let binding = ctx.generate_uid_in_root_scope("c", SymbolFlags::FunctionScopedVariable);
        let target = binding.create_target(ReferenceFlags::Write, ctx);
        self.registrations.push((binding, persistent_id));
        target
    }

    fn replace_inner_components(
        &mut self,
        inferred_name: &str,
        expr: &mut Expression<'a>,
        is_variable_declarator: bool,
        ctx: &mut TraverseCtx<'a>,
    ) -> bool {
        match expr {
            Expression::Identifier(ident) => {
                return is_componentish_name(&ident.name);
            }
            Expression::FunctionExpression(_) => {}
            Expression::ArrowFunctionExpression(arrow) => {
                if arrow
                    .get_expression()
                    .is_some_and(|expr| matches!(expr, Expression::ArrowFunctionExpression(_)))
                {
                    return false;
                }
            }
            Expression::CallExpression(call_expr) => {
                let allowed_callee = matches!(
                    call_expr.callee,
                    Expression::Identifier(_)
                        | Expression::ComputedMemberExpression(_)
                        | Expression::StaticMemberExpression(_)
                        | Expression::CallExpression(_)
                );

                if !allowed_callee {
                    return false;
                }

                let callee_source = call_expr.callee.span().source_text(ctx.state.source_text);
                let mut found_inside = false;
                for argument in &mut call_expr.arguments {
                    let Some(argument_expr) = argument.as_expression_mut() else {
                        continue;
                    };
                    found_inside |= self.replace_inner_components(
                        format!("{inferred_name}${callee_source}").as_str(),
                        argument_expr,
                        false,
                        ctx,
                    );
                }

                return found_inside;
            }
            _ => {
                return false;
            }
        }

        if !is_variable_declarator {
            *expr = ctx.ast.expression_assignment(
                SPAN,
                AssignmentOperator::Assign,
                self.create_registration(ctx.ast.atom(inferred_name), ctx),
                expr.take_in(ctx.ast),
            );
        }

        true
    }

    fn create_assignment_expression(
        &mut self,
        id: &BindingIdentifier<'a>,
        ctx: &mut TraverseCtx<'a>,
    ) -> Statement<'a> {
        let left = self.create_registration(id.name.into(), ctx);
        let right =
            ctx.create_bound_ident_expr(SPAN, id.name, id.symbol_id(), ReferenceFlags::Read);
        let expr = ctx.ast.expression_assignment(SPAN, AssignmentOperator::Assign, left, right);
        ctx.ast.statement_expression(SPAN, expr)
    }

    fn create_signature_call_expression(
        &mut self,
        scope_id: ScopeId,
        body: &mut FunctionBody<'a>,
        ctx: &mut TraverseCtx<'a>,
    ) -> Option<(BindingIdentifier<'a>, ArenaVec<'a, Argument<'a>>)> {
        let key = self.function_signature_keys.remove(&scope_id)?;

        let key = if self.emit_full_signatures {
            ctx.ast.atom(&key)
        } else {
            const SHA1_HASH_LEN: usize = 20;
            const ENCODED_LEN: usize = {
                let len = base64_encoded_len(SHA1_HASH_LEN, true);
                match len {
                    Some(l) => l,
                    None => panic!("Invalid base64 length"),
                }
            };

            let mut hasher = Sha1::new();
            hasher.update(&key);
            let hash = hasher.finalize();
            debug_assert_eq!(hash.len(), SHA1_HASH_LEN);

            const ZEROS_STR: &str = {
                const ZEROS_BYTES: [u8; ENCODED_LEN] = [0; ENCODED_LEN];
                match str::from_utf8(&ZEROS_BYTES) {
                    Ok(s) => s,
                    Err(_) => unreachable!(),
                }
            };

            let mut hashed_key = ArenaStringBuilder::from_str_in(ZEROS_STR, ctx.ast.allocator);
            let hashed_key_bytes = unsafe { hashed_key.as_mut_str().as_bytes_mut() };
            let encoded_bytes = BASE64_STANDARD.encode_slice(hash, hashed_key_bytes).unwrap();
            debug_assert_eq!(encoded_bytes, ENCODED_LEN);
            Atom::from(hashed_key)
        };

        let callee_list = self.non_builtin_hooks_callee.remove(&scope_id).unwrap_or_default();
        let callee_len = callee_list.len();
        let custom_hooks_in_scope = ctx.ast.vec_from_iter(
            callee_list.into_iter().filter_map(|e| e.map(ArrayExpressionElement::from)),
        );

        let force_reset = custom_hooks_in_scope.len() != callee_len;

        let mut arguments = ctx.ast.vec();
        arguments.push(Argument::from(ctx.ast.expression_string_literal(SPAN, key, None)));

        if force_reset || !custom_hooks_in_scope.is_empty() {
            arguments.push(Argument::from(ctx.ast.expression_boolean_literal(SPAN, force_reset)));
        }

        if !custom_hooks_in_scope.is_empty() {
            let formal_parameters = ctx.ast.formal_parameters(
                SPAN,
                FormalParameterKind::FormalParameter,
                ctx.ast.vec(),
                NONE,
            );
            let function_body = ctx.ast.function_body(
                SPAN,
                ctx.ast.vec(),
                ctx.ast.vec1(ctx.ast.statement_return(
                    SPAN,
                    Some(ctx.ast.expression_array(SPAN, custom_hooks_in_scope)),
                )),
            );
            let scope_id = ctx.create_child_scope_of_current(ScopeFlags::Function);
            let function =
                Argument::from(ctx.ast.expression_function_with_scope_id_and_pure_and_pife(
                    SPAN,
                    FunctionType::FunctionExpression,
                    None,
                    false,
                    false,
                    false,
                    NONE,
                    NONE,
                    formal_parameters,
                    NONE,
                    Some(function_body),
                    scope_id,
                    false,
                    false,
                ));
            arguments.push(function);
        }

        let init = ctx.ast.expression_call(
            SPAN,
            self.refresh_sig.to_expression(ctx),
            NONE,
            ctx.ast.vec(),
            false,
        );
        let binding = VarDeclarationsStore::create_uid_var_with_init("s", init, ctx);

        let call_expression = ctx.ast.statement_expression(
            SPAN,
            ctx.ast.expression_call(
                SPAN,
                binding.create_read_expression(ctx),
                NONE,
                ctx.ast.vec(),
                false,
            ),
        );

        body.statements.insert(0, call_expression);

        let binding_identifier = binding.create_binding_identifier(ctx);
        Some((binding_identifier, arguments))
    }

    fn process_statement(
        &mut self,
        statement: &mut Statement<'a>,
        ctx: &mut TraverseCtx<'a>,
    ) -> Option<Statement<'a>> {
        match statement {
            Statement::VariableDeclaration(variable) => {
                self.handle_variable_declaration(variable, ctx)
            }
            Statement::FunctionDeclaration(func) => self.handle_function_declaration(func, ctx),
            Statement::ClassDeclaration(class) => self.handle_class_declaration(class, ctx),
            Statement::ExportNamedDeclaration(export_decl) => {
                if let Some(declaration) = &mut export_decl.declaration {
                    match declaration {
                        Declaration::FunctionDeclaration(func) => {
                            self.handle_function_declaration(func, ctx)
                        }
                        Declaration::VariableDeclaration(variable) => {
                            self.handle_variable_declaration(variable, ctx)
                        }
                        Declaration::ClassDeclaration(class) => {
                            self.handle_class_declaration(class, ctx)
                        }
                        _ => None,
                    }
                } else {
                    None
                }
            }
            Statement::ExportDefaultDeclaration(stmt_decl) => match &mut stmt_decl.declaration {
                declaration @ match_expression!(ExportDefaultDeclarationKind) => {
                    let expression = declaration.to_expression_mut();
                    if !matches!(expression, Expression::CallExpression(_)) {
                        return None;
                    }

                    self.replace_inner_components("%default%", expression, false, ctx);
                    None
                }
                ExportDefaultDeclarationKind::FunctionDeclaration(func) => {
                    let id = func.id.as_ref()?;
                    if func.is_typescript_syntax() || !is_componentish_name(&id.name) {
                        return None;
                    }
                    Some(self.create_assignment_expression(id, ctx))
                }
                ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                    let id = class.id.as_ref()?;
                    if class.declare || !is_componentish_name(&id.name) {
                        return None;
                    }
                    Some(self.create_assignment_expression(id, ctx))
                }
                _ => None,
            },
            _ => None,
        }
    }

    fn handle_function_declaration(
        &mut self,
        func: &Function<'a>,
        ctx: &mut TraverseCtx<'a>,
    ) -> Option<Statement<'a>> {
        let Some(id) = &func.id else {
            return None;
        };

        if func.is_typescript_syntax() || !is_componentish_name(&id.name) {
            return None;
        }

        Some(self.create_assignment_expression(id, ctx))
    }

    fn handle_class_declaration(
        &mut self,
        class: &Class<'a>,
        ctx: &mut TraverseCtx<'a>,
    ) -> Option<Statement<'a>> {
        let Some(id) = &class.id else {
            return None;
        };

        if class.declare || !is_componentish_name(&id.name) {
            return None;
        }

        Some(self.create_assignment_expression(id, ctx))
    }

    fn handle_variable_declaration(
        &mut self,
        decl: &mut VariableDeclaration<'a>,
        ctx: &mut TraverseCtx<'a>,
    ) -> Option<Statement<'a>> {
        if decl.declarations.len() != 1 {
            return None;
        }

        let declarator = decl.declarations.first_mut().unwrap_or_else(|| unreachable!());
        let init = declarator.init.as_mut()?;
        let id = declarator.id.get_binding_identifier()?;
        let symbol_id = id.symbol_id();

        if !is_componentish_name(&id.name) {
            return None;
        }

        match init {
            Expression::ArrowFunctionExpression(arrow) => {
                if arrow
                    .get_expression()
                    .is_some_and(|expr| matches!(expr, Expression::ArrowFunctionExpression(_)))
                {
                    return None;
                }
            }
            Expression::FunctionExpression(_) | Expression::TaggedTemplateExpression(_) => {}
            Expression::CallExpression(call_expr) => {
                let is_import_expression = match call_expr.callee.get_inner_expression() {
                    Expression::ImportExpression(_) => true,
                    Expression::Identifier(ident) => {
                        ident.name.starts_with("require") || ident.name.starts_with("import")
                    }
                    _ => false,
                };

                if is_import_expression {
                    return None;
                }
            }
            _ => {
                return None;
            }
        }

        let found_inside = self.replace_inner_components(&id.name, init, true, ctx);

        if !found_inside && !self.used_in_jsx_bindings.contains(&symbol_id) {
            return None;
        }

        Some(self.create_assignment_expression(id, ctx))
    }

    fn handle_function_in_variable_declarator(
        &self,
        id_binding: &BoundIdentifier<'a>,
        binding: &BoundIdentifier<'a>,
        mut arguments: ArenaVec<'a, Argument<'a>>,
        ctx: &mut TraverseCtx<'a>,
    ) {
        arguments.insert(0, Argument::from(id_binding.create_read_expression(ctx)));
        let statement = ctx.ast.statement_expression(
            SPAN,
            ctx.ast.expression_call(
                SPAN,
                binding.create_read_expression(ctx),
                NONE,
                arguments,
                false,
            ),
        );

        let address =
            if let Ancestor::ExportNamedDeclarationDeclaration(export_decl) = ctx.ancestor(2) {
                export_decl.address()
            } else {
                let var_decl = ctx.ancestor(1);
                debug_assert!(matches!(var_decl, Ancestor::VariableDeclarationDeclarations(_)));
                var_decl.address()
            };
        ctx.state.statement_injector.insert_after(&address, statement);
    }

    fn transform_arrow_function_to_block(
        arrow: &mut ArrowFunctionExpression<'a>,
        ctx: &TraverseCtx<'a>,
    ) {
        if !arrow.expression {
            return;
        }

        arrow.expression = false;

        let Some(Statement::ExpressionStatement(statement)) = arrow.body.statements.pop() else {
            unreachable!("arrow function body is never empty")
        };

        arrow
            .body
            .statements
            .push(ctx.ast.statement_return(SPAN, Some(statement.unbox().expression)));
    }

    fn collect_create_context_bindings(&mut self, program: &Program<'a>) {
        self.create_context_bindings.clear();

        for statement in &program.body {
            let Statement::ImportDeclaration(import_decl) = statement else { continue };
            if !matches!(import_decl.source.value.as_str(), "preact" | "react" | "preact/compat") {
                continue;
            }
            let Some(specifiers) = &import_decl.specifiers else { continue };
            for specifier in specifiers {
                let ImportDeclarationSpecifier::ImportSpecifier(specifier) = specifier else {
                    continue;
                };
                if specifier.imported.name() == "createContext" {
                    self.create_context_bindings.insert(specifier.local.symbol_id());
                }
            }
        }
    }

    fn context_filename(ctx: &TraverseCtx<'a>) -> String {
        if let Some(path) = ctx.state.source_path.to_str()
            && !path.is_empty()
        {
            return path.to_string();
        }
        if !ctx.state.filename.is_empty() {
            return ctx.state.filename.clone();
        }
        String::from("unnamed")
    }

    fn is_create_context_call(
        &self,
        call_expr: &CallExpression<'a>,
        ctx: &TraverseCtx<'a>,
    ) -> bool {
        let Expression::Identifier(ident) = call_expr.callee.get_inner_expression() else {
            return false;
        };
        let Some(symbol_id) = ctx.scoping().get_reference(ident.reference_id()).symbol_id() else {
            return false;
        };
        self.create_context_bindings.contains(&symbol_id)
    }

    fn transform_create_context_call(
        &mut self,
        call_expr: &CallExpression<'a>,
        ctx: &mut TraverseCtx<'a>,
    ) -> Expression<'a> {
        let mut suffix = match ctx.parent() {
            Ancestor::ObjectPropertyValue(property) => {
                property.key().static_name().map_or_else(String::new, |name| format!("__{name}"))
            }
            Ancestor::VariableDeclaratorInit(declarator) => declarator
                .id()
                .get_binding_identifier()
                .map_or_else(String::new, |id| format!("${}", id.name)),
            Ancestor::AssignmentExpressionRight(assignment) => {
                assignment.left().get_identifier_name().map_or_else(
                    || {
                        format!(
                            "_{}",
                            hash_base36(
                                assignment.left().span().source_text(ctx.state.source_text)
                            )
                        )
                    },
                    |name| format!("_{name}"),
                )
            }
            _ => String::new(),
        };

        let counter = self.contexts.entry(suffix.clone()).or_insert(0);
        if *counter > 0 {
            suffix.push_str(&counter.to_string());
        }
        *counter += 1;

        let prefix = format!("_{}{}", self.file_hash, suffix);
        let id = self.build_context_identifier_expression(&prefix, ctx);
        let callee = call_expr.callee.clone_in(ctx.ast.allocator);
        let callee_for_member = callee.clone_in(ctx.ast.allocator);
        let callee_for_assign = callee.clone_in(ctx.ast.allocator);

        let member_left = Expression::from(ctx.ast.member_expression_computed(
            SPAN,
            callee_for_member,
            id.clone_in(ctx.ast.allocator),
            false,
        ));

        if let Some(argument) = call_expr.arguments.first().and_then(Argument::as_expression) {
            let value = clone_first_non_ts_expression(argument, ctx.ast);
            let create_context_call = ctx.ast.expression_call(
                SPAN,
                callee_for_assign,
                NONE,
                ctx.ast.vec1(Argument::from(value.clone_in(ctx.ast.allocator))),
                false,
            );
            let member_assign = AssignmentTarget::from(ctx.ast.member_expression_computed(
                SPAN,
                callee.clone_in(ctx.ast.allocator),
                id.clone_in(ctx.ast.allocator),
                false,
            ));
            let assign = ctx.ast.expression_assignment(
                SPAN,
                AssignmentOperator::Assign,
                member_assign,
                create_context_call,
            );
            let init = ctx.ast.expression_logical(SPAN, member_left, LogicalOperator::Or, assign);

            let property = ctx.ast.object_property_kind_object_property(
                SPAN,
                PropertyKind::Init,
                ctx.ast.property_key_static_identifier(SPAN, "__"),
                value,
                false,
                false,
                false,
            );
            let object = ctx.ast.expression_object(SPAN, ctx.ast.vec1(property));
            let object_assign = self.object_assign_callee(ctx);
            ctx.ast.expression_call(
                SPAN,
                object_assign,
                NONE,
                ctx.ast.vec_from_array([Argument::from(init), Argument::from(object)]),
                false,
            )
        } else {
            let create_context_call =
                ctx.ast.expression_call(SPAN, callee_for_assign, NONE, ctx.ast.vec(), false);
            let member_assign =
                AssignmentTarget::from(ctx.ast.member_expression_computed(SPAN, callee, id, false));
            let assign = ctx.ast.expression_assignment(
                SPAN,
                AssignmentOperator::Assign,
                member_assign,
                create_context_call,
            );
            ctx.ast.expression_logical(SPAN, member_left, LogicalOperator::Or, assign)
        }
    }

    fn object_assign_callee(&self, ctx: &mut TraverseCtx<'a>) -> Expression<'a> {
        let reference_id = ctx.create_unbound_reference("Object".into(), ReferenceFlags::Read);
        let object = ctx.ast.expression_identifier_with_reference_id(SPAN, "Object", reference_id);
        Expression::from(ctx.ast.member_expression_static(
            SPAN,
            object,
            ctx.ast.identifier_name(SPAN, "assign"),
            false,
        ))
    }

    fn build_context_identifier_expression(
        &self,
        prefix: &str,
        ctx: &mut TraverseCtx<'a>,
    ) -> Expression<'a> {
        let params = self.closest_context_params(ctx);
        if params.is_empty() {
            return ctx.ast.expression_string_literal(SPAN, ctx.ast.atom(prefix), None);
        }

        let mut quasis = ctx.ast.vec_with_capacity(params.len() + 1);
        quasis.push(ctx.ast.template_element(
            SPAN,
            TemplateElementValue { raw: ctx.ast.atom(prefix), cooked: Some(ctx.ast.atom(prefix)) },
            false,
            false,
        ));

        let mut expressions = ctx.ast.vec_with_capacity(params.len());
        for (index, (name, symbol_id)) in params.iter().enumerate() {
            expressions.push(ctx.create_bound_ident_expr(
                SPAN,
                *name,
                *symbol_id,
                ReferenceFlags::Read,
            ));
            quasis.push(ctx.ast.template_element(
                SPAN,
                TemplateElementValue { raw: ctx.ast.atom(""), cooked: Some(ctx.ast.atom("")) },
                index + 1 == params.len(),
                false,
            ));
        }

        ctx.ast.expression_template_literal(SPAN, quasis, expressions)
    }

    fn closest_context_params(&self, ctx: &TraverseCtx<'a>) -> Vec<(Ident<'a>, SymbolId)> {
        for ancestor in ctx.ancestors() {
            match ancestor {
                Ancestor::ArrowFunctionExpressionBody(arrow) => {
                    return collect_identifier_params(arrow.params());
                }
                Ancestor::FunctionBody(func)
                    if matches!(func.r#type(), FunctionType::FunctionDeclaration) =>
                {
                    return collect_identifier_params(func.params());
                }
                Ancestor::ProgramBody(_)
                | Ancestor::ProgramDirectives(_)
                | Ancestor::ProgramHashbang(_) => {
                    break;
                }
                _ => {}
            }
        }
        Vec::new()
    }
}

fn collect_identifier_params<'a>(params: &FormalParameters<'a>) -> Vec<(Ident<'a>, SymbolId)> {
    params
        .items
        .iter()
        .filter_map(|param| {
            param.pattern.get_binding_identifier().map(|id| (id.name, id.symbol_id()))
        })
        .collect()
}

fn clone_first_non_ts_expression<'a>(expr: &Expression<'a>, ast: AstBuilder<'a>) -> Expression<'a> {
    match expr {
        Expression::ParenthesizedExpression(parenthesized) => {
            clone_first_non_ts_expression(&parenthesized.expression, ast)
        }
        Expression::TSAsExpression(ts_as) => clone_first_non_ts_expression(&ts_as.expression, ast),
        Expression::TSSatisfiesExpression(ts_satisfies) => {
            clone_first_non_ts_expression(&ts_satisfies.expression, ast)
        }
        Expression::TSNonNullExpression(ts_non_null) => {
            clone_first_non_ts_expression(&ts_non_null.expression, ast)
        }
        _ => expr.clone_in(ast.allocator),
    }
}

fn is_componentish_name(name: &str) -> bool {
    name.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
}

fn is_use_hook_name(name: &str) -> bool {
    name.starts_with("use") && name.as_bytes().get(3).is_none_or(u8::is_ascii_uppercase)
}

fn is_builtin_hook(hook_name: &str) -> bool {
    matches!(
        hook_name,
        "useErrorBoundary"
            | "useState"
            | "useReducer"
            | "useEffect"
            | "useLayoutEffect"
            | "useMemo"
            | "useCallback"
            | "useRef"
            | "useContext"
            | "useImperativeMethods"
            | "useImperativeHandle"
            | "useDebugValue"
            | "useSignal"
    )
}

fn hash_base36(input: &str) -> String {
    let mut hash = 5381_u32;
    for ch in input.bytes().rev() {
        hash = hash.wrapping_mul(33) ^ u32::from(ch);
    }

    if hash == 0 {
        return String::from("0");
    }

    let mut hash = hash;
    let mut buf = [0_u8; 8];
    let mut index = buf.len();
    while hash > 0 {
        let digit = (hash % 36) as u8;
        index -= 1;
        buf[index] = if digit < 10 { b'0' + digit } else { b'a' + (digit - 10) };
        hash /= 36;
    }
    String::from_utf8_lossy(&buf[index..]).into_owned()
}

struct UsedInJSXBindingsCollector<'a, 'b> {
    ctx: &'b TraverseCtx<'a>,
    bindings: FxHashSet<SymbolId>,
}

impl<'a, 'b> UsedInJSXBindingsCollector<'a, 'b> {
    fn collect(program: &Program<'a>, ctx: &'b TraverseCtx<'a>) -> FxHashSet<SymbolId> {
        let mut visitor = Self { ctx, bindings: FxHashSet::default() };
        visitor.visit_program(program);
        visitor.bindings
    }

    fn is_jsx_like_call(name: &str) -> bool {
        matches!(name, "createElement" | "jsx" | "jsxDEV" | "jsxs")
    }
}

impl<'a> Visit<'a> for UsedInJSXBindingsCollector<'a, '_> {
    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        walk_call_expression(self, it);

        let is_jsx_call = match &it.callee {
            Expression::Identifier(ident) => Self::is_jsx_like_call(&ident.name),
            Expression::StaticMemberExpression(member) => {
                Self::is_jsx_like_call(&member.property.name)
            }
            _ => false,
        };

        if is_jsx_call
            && let Some(Argument::Identifier(ident)) = it.arguments.first()
            && let Some(symbol_id) =
                self.ctx.scoping().get_reference(ident.reference_id()).symbol_id()
        {
            self.bindings.insert(symbol_id);
        }
    }

    fn visit_jsx_opening_element(&mut self, it: &JSXOpeningElement<'_>) {
        if let Some(ident) = it.name.get_identifier()
            && let Some(symbol_id) =
                self.ctx.scoping().get_reference(ident.reference_id()).symbol_id()
        {
            self.bindings.insert(symbol_id);
        }
    }

    fn visit_ts_type_annotation(&mut self, _it: &TSTypeAnnotation<'a>) {}

    fn visit_declaration(&mut self, it: &Declaration<'a>) {
        if matches!(
            it,
            Declaration::TSTypeAliasDeclaration(_) | Declaration::TSInterfaceDeclaration(_)
        ) {
            return;
        }
        walk_declaration(self, it);
    }

    fn visit_import_declaration(&mut self, _it: &ImportDeclaration<'a>) {}

    fn visit_export_all_declaration(&mut self, _it: &ExportAllDeclaration<'a>) {}
}
