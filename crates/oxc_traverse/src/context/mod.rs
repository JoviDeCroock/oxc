use oxc_allocator::{Allocator, Box};
use oxc_ast::AstBuilder;
use oxc_semantic::{ScopeTree, SymbolTable};
use oxc_syntax::scope::{ScopeFlags, ScopeId};

use crate::ancestor::{Ancestor, AncestorType};

mod ancestry;
pub use ancestry::TraverseAncestry;
mod scoping;
pub use scoping::TraverseScoping;

/// Traverse context.
///
/// Passed to all AST visitor functions.
///
/// Provides ability to:
/// * Query parent/ancestor of current node via [`parent`], [`ancestor`], [`find_ancestor`].
/// * Get scopes tree and symbols table via [`scopes`], [`symbols`], [`scopes_mut`], [`symbols_mut`],
///   [`find_scope`], [`find_scope_by_flags`].
/// * Create AST nodes via AST builder [`ast`].
/// * Allocate into arena via [`alloc`].
///
/// # Namespaced APIs
///
/// All APIs are provided via 2 routes:
///
/// 1. Directly on `TraverseCtx`.
/// 2. Via "namespaces".
///
/// | Direct                   | Namespaced                       |
/// |--------------------------|----------------------------------|
/// | `ctx.parent()`           | `ctx.ancestry.parent()`          |
/// | `ctx.current_scope_id()` | `ctx.scoping.current_scope_id()` |
/// | `ctx.alloc(thing)`       | `ctx.ast.alloc(thing)`           |
///
/// Purpose of the "namespaces" is to support if you want to mutate scope tree or symbol table
/// while holding an `&Ancestor`, or AST nodes obtained from an `&Ancestor`.
///
/// For example, this will not compile because it attempts to borrow `ctx`
/// immutably and mutably at same time:
///
/// ```nocompile
/// use oxc_ast::ast::*;
/// use oxc_traverse::{Ancestor, Traverse, TraverseCtx};
///
/// struct MyTransform;
/// impl<'a> Traverse<'a> for MyTransform {
///     fn enter_unary_expression(&mut self, unary_expr: &mut UnaryExpression<'a>, ctx: &mut TraverseCtx<'a>) {
///         // `right` is ultimately borrowed from `ctx`
///         let right = match ctx.parent() {
///             Ancestor::BinaryExpressionLeft(bin_expr) => bin_expr.right(),
///             _ => return,
///         };
///
///         // Won't compile! `ctx.scopes_mut()` attempts to mut borrow `ctx`
///         // while it's already borrowed by `right`.
///         let scope_tree_mut = ctx.scopes_mut();
///
///         // Use `right` later on
///         dbg!(right);
///     }
/// }
/// ```
///
/// You can fix this by using the "namespaced" methods instead.
/// This works because you can borrow `ctx.ancestry` and `ctx.scoping` simultaneously:
///
/// ```
/// use oxc_ast::ast::*;
/// use oxc_traverse::{Ancestor, Traverse, TraverseCtx};
///
/// struct MyTransform;
/// impl<'a> Traverse<'a> for MyTransform {
///     fn enter_unary_expression(&mut self, unary_expr: &mut UnaryExpression<'a>, ctx: &mut TraverseCtx<'a>) {
///         let right = match ctx.ancestry.parent() {
///             Ancestor::BinaryExpressionLeft(bin_expr) => bin_expr.right(),
///             _ => return,
///         };
///
///         let scope_tree_mut = ctx.scoping.scopes_mut();
///
///         dbg!(right);
///     }
/// }
/// ```
///
/// [`parent`]: `TraverseCtx::parent`
/// [`ancestor`]: `TraverseCtx::ancestor`
/// [`find_ancestor`]: `TraverseCtx::find_ancestor`
/// [`scopes`]: `TraverseCtx::scopes`
/// [`symbols`]: `TraverseCtx::symbols`
/// [`scopes_mut`]: `TraverseCtx::scopes_mut`
/// [`symbols_mut`]: `TraverseCtx::symbols_mut`
/// [`find_scope`]: `TraverseCtx::find_scope`
/// [`find_scope_by_flags`]: `TraverseCtx::find_scope_by_flags`
/// [`ast`]: `TraverseCtx::ast`
/// [`alloc`]: `TraverseCtx::alloc`
pub struct TraverseCtx<'a> {
    pub ancestry: TraverseAncestry<'a>,
    pub scoping: TraverseScoping,
    pub ast: AstBuilder<'a>,
}

/// Return value of closure when using [`TraverseCtx::find_ancestor`] or [`TraverseCtx::find_scope`].
pub enum FinderRet<T> {
    Found(T),
    Stop,
    Continue,
}

// Public methods
impl<'a> TraverseCtx<'a> {
    /// Create new traversal context.
    pub(crate) fn new(scopes: ScopeTree, symbols: SymbolTable, allocator: &'a Allocator) -> Self {
        let ancestry = TraverseAncestry::new();
        let scoping = TraverseScoping::new(scopes, symbols);
        let ast = AstBuilder::new(allocator);
        Self { ancestry, scoping, ast }
    }

    /// Allocate a node in the arena.
    ///
    /// Returns a [`Box<T>`].
    ///
    /// Shortcut for `ctx.ast.alloc`.
    #[inline]
    pub fn alloc<T>(&self, node: T) -> Box<'a, T> {
        self.ast.alloc(node)
    }

    /// Get parent of current node.
    ///
    /// Shortcut for `ctx.ancestry.parent`.
    #[inline]
    #[allow(unsafe_code)]
    pub fn parent(&self) -> &Ancestor<'a> {
        self.ancestry.parent()
    }

    /// Get ancestor of current node.
    ///
    /// `level` is number of levels above.
    /// `ancestor(1).unwrap()` is equivalent to `parent()`.
    ///
    /// Shortcut for `ctx.ancestry.ancestor`.
    #[inline]
    pub fn ancestor(&self, level: usize) -> Option<&Ancestor<'a>> {
        self.ancestry.ancestor(level)
    }

    /// Walk up trail of ancestors to find a node.
    ///
    /// `finder` should return:
    /// * `FinderRet::Found(value)` to stop walking and return `Some(value)`.
    /// * `FinderRet::Stop` to stop walking and return `None`.
    /// * `FinderRet::Continue` to continue walking up.
    ///
    /// # Example
    ///
    /// ```
    /// use oxc_ast::ast::ThisExpression;
    /// use oxc_traverse::{Ancestor, FinderRet, Traverse, TraverseCtx};
    ///
    /// struct MyTraverse;
    /// impl<'a> Traverse<'a> for MyTraverse {
    ///     fn enter_this_expression(&mut self, this_expr: &mut ThisExpression, ctx: &mut TraverseCtx<'a>) {
    ///         // Get name of function where `this` is bound.
    ///         // NB: This example doesn't handle `this` in class fields or static blocks.
    ///         let fn_id = ctx.find_ancestor(|ancestor| {
    ///             match ancestor {
    ///                 Ancestor::FunctionBody(func) => FinderRet::Found(func.id()),
    ///                 Ancestor::FunctionParams(func) => FinderRet::Found(func.id()),
    ///                 _ => FinderRet::Continue
    ///             }
    ///         });
    ///     }
    /// }
    /// ```
    ///
    /// Shortcut for `self.ancestry.find_ancestor`.
    pub fn find_ancestor<'c, F, O>(&'c self, finder: F) -> Option<O>
    where
        F: Fn(&'c Ancestor<'a>) -> FinderRet<O>,
    {
        self.ancestry.find_ancestor(finder)
    }

    /// Get depth in the AST.
    ///
    /// Count includes current node. i.e. in `Program`, depth is 1.
    ///
    /// Shortcut for `self.ancestry.ancestors_depth`.
    #[inline]
    pub fn ancestors_depth(&self) -> usize {
        self.ancestry.ancestors_depth()
    }

    /// Get current scope ID.
    ///
    /// Shortcut for `ctx.scoping.current_scope_id`.
    #[inline]
    pub fn current_scope_id(&self) -> ScopeId {
        self.scoping.current_scope_id()
    }

    /// Get scopes tree.
    ///
    /// Shortcut for `ctx.scoping.scopes`.
    #[inline]
    pub fn scopes(&self) -> &ScopeTree {
        self.scoping.scopes()
    }

    /// Get mutable scopes tree.
    ///
    /// Shortcut for `ctx.scoping.scopes_mut`.
    #[inline]
    pub fn scopes_mut(&mut self) -> &mut ScopeTree {
        self.scoping.scopes_mut()
    }

    /// Get symbols table.
    ///
    /// Shortcut for `ctx.scoping.symbols`.
    #[inline]
    pub fn symbols(&self) -> &SymbolTable {
        self.scoping.symbols()
    }

    /// Get mutable symbols table.
    ///
    /// Shortcut for `ctx.scoping.symbols_mut`.
    #[inline]
    pub fn symbols_mut(&mut self) -> &mut SymbolTable {
        self.scoping.symbols_mut()
    }

    /// Walk up trail of scopes to find a scope.
    ///
    /// `finder` is called with `ScopeId`.
    ///
    /// `finder` should return:
    /// * `FinderRet::Found(value)` to stop walking and return `Some(value)`.
    /// * `FinderRet::Stop` to stop walking and return `None`.
    /// * `FinderRet::Continue` to continue walking up.
    ///
    /// This is a shortcut for `ctx.scoping.find_scope`.
    pub fn find_scope<F, O>(&self, finder: F) -> Option<O>
    where
        F: Fn(ScopeId) -> FinderRet<O>,
    {
        self.scoping.find_scope(finder)
    }

    /// Walk up trail of scopes to find a scope by checking `ScopeFlags`.
    ///
    /// `finder` is called with `ScopeFlags`.
    ///
    /// `finder` should return:
    /// * `FinderRet::Found(value)` to stop walking and return `Some(value)`.
    /// * `FinderRet::Stop` to stop walking and return `None`.
    /// * `FinderRet::Continue` to continue walking up.
    ///
    /// This is a shortcut for `ctx.scoping.find_scope_by_flags`.
    pub fn find_scope_by_flags<F, O>(&self, finder: F) -> Option<O>
    where
        F: Fn(ScopeFlags) -> FinderRet<O>,
    {
        self.scoping.find_scope_by_flags(finder)
    }
}

// Methods used internally within crate
impl<'a> TraverseCtx<'a> {
    /// Shortcut for `self.ancestry.push_stack`, to make `walk_*` methods less verbose.
    ///
    /// # SAFETY
    /// This method must not be public outside this crate, or consumer could break safety invariants.
    #[inline]
    pub(crate) fn push_stack(&mut self, ancestor: Ancestor<'a>) {
        self.ancestry.push_stack(ancestor);
    }

    /// Shortcut for `self.ancestry.pop_stack`, to make `walk_*` methods less verbose.
    ///
    /// # SAFETY
    /// See safety constraints of `TraverseAncestry.pop_stack`.
    /// This method must not be public outside this crate, or consumer could break safety invariants.
    #[inline]
    #[allow(unsafe_code)]
    pub(crate) unsafe fn pop_stack(&mut self) {
        self.ancestry.pop_stack();
    }

    /// Shortcut for `self.ancestry.retag_stack`, to make `walk_*` methods less verbose.
    ///
    /// # SAFETY
    /// See safety constraints of `TraverseAncestry.retag_stack`.
    /// This method must not be public outside this crate, or consumer could break safety invariants.
    #[inline]
    #[allow(unsafe_code)]
    pub(crate) unsafe fn retag_stack(&mut self, ty: AncestorType) {
        self.ancestry.retag_stack(ty);
    }

    /// Shortcut for `ctx.scoping.set_current_scope_id`, to make `walk_*` methods less verbose.
    #[inline]
    pub(crate) fn set_current_scope_id(&mut self, scope_id: ScopeId) {
        self.scoping.set_current_scope_id(scope_id);
    }
}
