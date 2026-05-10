use oxc_ast::ast::*;
use oxc_span::GetSpan;

use crate::{
    ast_nodes::{AstNode, AstNodes},
    formatter::{Formatter, prelude::*, trivia::FormatTrailingComments},
    parentheses::NeedsParentheses,
    print::{FormatNodeWithoutTrailingComments, FormatWrite},
    write,
};

impl<'a> FormatWrite<'a> for AstNode<'a, TSAsExpression<'a>> {
    fn write(&self, f: &mut Formatter<'_, 'a>) {
        let is_callee_or_object = is_callee_or_object_context(self.span(), self.parent());
        let needs_parentheses = self.needs_parentheses(f);
        format_as_or_satisfies_expression(
            self.expression(),
            self.type_annotation(),
            is_callee_or_object,
            needs_parentheses,
            "as",
            f,
        );
    }
}

impl<'a> FormatWrite<'a> for AstNode<'a, TSSatisfiesExpression<'a>> {
    fn write(&self, f: &mut Formatter<'_, 'a>) {
        let is_callee_or_object = is_callee_or_object_context(self.span(), self.parent());
        let needs_parentheses = self.needs_parentheses(f);
        format_as_or_satisfies_expression(
            self.expression(),
            self.type_annotation(),
            is_callee_or_object,
            needs_parentheses,
            "satisfies",
            f,
        );
    }
}

fn format_as_or_satisfies_expression<'a>(
    expression: &AstNode<'a, Expression>,
    type_annotation: &AstNode<'a, TSType>,
    is_callee_or_object: bool,
    needs_parentheses: bool,
    operation: &'static str,
    f: &mut Formatter<'_, 'a>,
) {
    let format_inner = format_with(|f| {
        let type_start = type_annotation.span().start;

        // Check for block comments between expression and type.
        // Prettier's `handleBinaryCastExpressionComment()` handles these specially.
        // https://github.com/prettier/prettier/blob/fdfa6701767f5140a85902ecc9fb6444f5b4e3f8/src/language-js/comments/handle-comments.js#L1131
        // See also https://github.com/prettier/prettier/blob/3.7.3/tests/format/typescript/as/comments/18160.ts
        let comments = f.context().comments().comments_in_range(expression.span().end, type_start);
        let multiline_comment_position = comments.iter().position(|c| c.is_multiline_block());
        let block_comments =
            if let Some(pos) = multiline_comment_position { &comments[..pos] } else { comments };

        if !comments.is_empty()
            && let AstNodes::TSTypeReference(reference) = type_annotation.as_ast_nodes()
            && reference.type_name.is_const()
        {
            write!(f, [FormatNodeWithoutTrailingComments(expression)]);
            write!(f, [FormatTrailingComments::Comments(block_comments)]);
            write!(f, [space(), token(operation), space(), token("const")]);
        } else if block_comments.is_empty() {
            write!(f, [FormatNodeWithoutTrailingComments(expression)]);
            write!(f, [space(), token(operation), space(), type_annotation]);
        } else {
            write!(f, [expression, space(), token(operation), space(), type_annotation]);
        }
    });

    // When the formatter wraps the expression in parentheses and there are leading
    // line comments on the inner expression (which the auto-`fmt` no longer prints
    // for `TSAsExpression` / `TSSatisfiesExpression`), the comments must end up
    // inside the inserted parens. Without `soft_block_indent`, the leading comment
    // would be glued to the opening paren on the same line (`(// comment\n expr)`),
    // and oxlint-disable directives could end up applying to the wrong line.
    let has_leading_comments_in_parens = needs_parentheses
        && !is_callee_or_object
        && !f.context().comments().comments_before(expression.span().start).is_empty();

    if is_callee_or_object || has_leading_comments_in_parens {
        write!(f, [group(&soft_block_indent(&format_inner))]);
    } else {
        write!(f, [format_inner]);
    }
}

fn is_callee_or_object_context(span: Span, parent: &AstNodes<'_>) -> bool {
    match parent {
        // Static member
        AstNodes::StaticMemberExpression(_) => true,
        AstNodes::ComputedMemberExpression(member) => member.object.span() == span,
        // Or CallExpression callee (Not NewExpression, to align with Prettier)
        // https://github.com/prettier/prettier/blob/fdfa6701767f5140a85902ecc9fb6444f5b4e3f8/src/language-js/print/cast-expression.js#L28-L33
        // NOTE: We may revert this if resolved: https://github.com/prettier/prettier/issues/18406
        // _ => parent.is_call_like_callee_span(span),
        AstNodes::CallExpression(call) => call.callee.span() == span,
        _ => false,
    }
}
