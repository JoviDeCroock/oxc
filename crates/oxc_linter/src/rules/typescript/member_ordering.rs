use std::{borrow::Cow, ops::Deref, sync::LazyLock};

use oxc_ast::{
    AstKind,
    ast::{
        AccessorProperty, AccessorPropertyType, Class, ClassElement, ClassType, Expression,
        MethodDefinition, MethodDefinitionKind, MethodDefinitionType, PropertyDefinition,
        PropertyDefinitionType, PropertyKey, TSAccessibility, TSIndexSignature,
        TSMethodSignatureKind, TSSignature, TSTypeLiteral,
    },
};
use oxc_diagnostics::OxcDiagnostic;
use oxc_macros::declare_oxc_lint;
use oxc_span::Span;
use rustc_hash::FxHashSet;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, de};

use crate::{
    AstNode,
    context::{ContextHost, LintContext},
    rule::{DefaultRuleConfig, Rule},
};

fn incorrect_group_order_diagnostic(span: Span, name: &str, rank: &str) -> OxcDiagnostic {
    OxcDiagnostic::warn(format!("Member {name} should be declared before all {rank} definitions."))
        .with_label(span)
}

fn incorrect_order_diagnostic(span: Span, member: &str, before_member: &str) -> OxcDiagnostic {
    OxcDiagnostic::warn(format!(
        "Member {member} should be declared before member {before_member}."
    ))
    .with_label(span)
}

fn incorrect_required_members_order_diagnostic(
    span: Span,
    member: &str,
    optional_or_required: &str,
) -> OxcDiagnostic {
    OxcDiagnostic::warn(format!(
        "Member {member} should be declared after all {optional_or_required} members."
    ))
    .with_label(span)
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemberOrdering(Box<MemberOrderingOptions>);

impl Default for MemberOrdering {
    fn default() -> Self {
        Self(Box::new(MemberOrderingOptions::default()))
    }
}

impl Deref for MemberOrdering {
    type Target = MemberOrderingOptions;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct MemberOrderingOptions {
    /// Order to enforce for all supported constructs, used as a fallback.
    default: OrderConfig,
    /// Order to enforce for class declarations.
    classes: Option<OrderConfig>,
    /// Order to enforce for class expressions.
    class_expressions: Option<OrderConfig>,
    /// Order to enforce for interfaces.
    interfaces: Option<OrderConfig>,
    /// Order to enforce for type literals.
    type_literals: Option<OrderConfig>,
}

impl Default for MemberOrderingOptions {
    fn default() -> Self {
        Self {
            default: OrderConfig::default(),
            classes: None,
            class_expressions: None,
            interfaces: None,
            type_literals: None,
        }
    }
}

#[derive(Debug, Clone, JsonSchema)]
#[serde(untagged)]
enum OrderConfig {
    Never(Never),
    MemberTypes(Vec<MemberTypeGroup>),
    Sorted(SortedOrderConfig),
}

impl Default for OrderConfig {
    fn default() -> Self {
        Self::Sorted(SortedOrderConfig {
            member_types: Some(MemberTypesOption::Types(default_member_types())),
            optionality_order: None,
            order: Order::AsWritten,
        })
    }
}

impl<'de> Deserialize<'de> for OrderConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let config = OrderConfig::deserialize_untagged(deserializer)?;
        Ok(config)
    }
}

impl OrderConfig {
    fn deserialize_untagged<'de, D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Never(Never),
            MemberTypes(Vec<MemberTypeGroup>),
            Sorted(SortedOrderConfig),
        }

        match Raw::deserialize(deserializer)? {
            Raw::Never(never) => Ok(Self::Never(never)),
            Raw::MemberTypes(member_types) => Ok(Self::MemberTypes(member_types)),
            Raw::Sorted(sorted) => Ok(Self::Sorted(sorted)),
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct SortedOrderConfig {
    /// Member type groups to enforce, or `"never"` to ignore member type groups.
    member_types: Option<MemberTypesOption>,
    /// Whether optional members should come before or after required members.
    optionality_order: Option<OptionalityOrder>,
    /// Name sorting order within member groups.
    order: Order,
}

impl Default for SortedOrderConfig {
    fn default() -> Self {
        Self { member_types: None, optionality_order: None, order: Order::AsWritten }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
enum MemberTypesOption {
    Never(Never),
    Types(Vec<MemberTypeGroup>),
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
enum MemberTypeGroup {
    Single(MemberTypeName),
    Group(Vec<MemberTypeName>),
}

impl MemberTypeGroup {
    fn contains(&self, candidate: &str) -> bool {
        match self {
            Self::Single(name) => name.as_str() == candidate,
            Self::Group(names) => names.iter().any(|name| name.as_str() == candidate),
        }
    }

    fn display(&self) -> String {
        match self {
            Self::Single(name) => name.display(),
            Self::Group(names) => {
                names.iter().map(MemberTypeName::display).collect::<Vec<_>>().join(", ")
            }
        }
    }
}

#[derive(Debug, Clone, JsonSchema)]
struct MemberTypeName(String);

impl MemberTypeName {
    fn as_str(&self) -> &str {
        &self.0
    }

    fn display(&self) -> String {
        self.0.replace('-', " ")
    }
}

impl<'de> Deserialize<'de> for MemberTypeName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if all_member_types().contains(&value) {
            Ok(Self(value))
        } else {
            Err(de::Error::custom(format!("unknown member type `{value}`")))
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
enum Never {
    Never,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
enum Order {
    Alphabetically,
    AlphabeticallyCaseInsensitive,
    #[default]
    AsWritten,
    Natural,
    NaturalCaseInsensitive,
}

impl Order {
    fn is_sorted(self) -> bool {
        !matches!(self, Self::AsWritten)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
enum OptionalityOrder {
    OptionalFirst,
    RequiredFirst,
}

declare_oxc_lint!(
    /// ### What it does
    ///
    /// Require a consistent member declaration order in classes, interfaces, and type literals.
    ///
    /// ### Why is this bad?
    ///
    /// Consistent ordering of fields, constructors, accessors, methods, and type signatures can
    /// make TypeScript APIs easier to read and navigate.
    ///
    /// ### Examples
    ///
    /// Examples of **incorrect** code for this rule with `{ "default": ["signature", "method", "constructor", "field"] }`:
    /// ```ts
    /// interface Foo {
    ///   B: string;
    ///   new (): Foo;
    ///   A(): void;
    ///   [Z: string]: unknown;
    /// }
    /// ```
    ///
    /// Examples of **correct** code for this rule:
    /// ```ts
    /// interface Foo {
    ///   [Z: string]: unknown;
    ///   A(): void;
    ///   new (): Foo;
    ///   B: string;
    /// }
    /// ```
    MemberOrdering,
    typescript,
    style,
    config = MemberOrderingOptions,
    version = "1.64.0",
);

impl Rule for MemberOrdering {
    fn from_configuration(value: serde_json::Value) -> Result<Self, serde_json::error::Error> {
        serde_json::from_value::<DefaultRuleConfig<Self>>(value).map(DefaultRuleConfig::into_inner)
    }

    fn run<'a>(&self, node: &AstNode<'a>, ctx: &LintContext<'a>) {
        match node.kind() {
            AstKind::Class(class) => {
                let config = match class.r#type {
                    ClassType::ClassDeclaration => self.classes.as_ref().unwrap_or(&self.default),
                    ClassType::ClassExpression => {
                        self.class_expressions.as_ref().unwrap_or(&self.default)
                    }
                };
                validate_members_order(&class_members(class), config, true, ctx);
            }
            AstKind::TSInterfaceDeclaration(interface) => {
                let config = self.interfaces.as_ref().unwrap_or(&self.default);
                validate_members_order(
                    &signature_members(&interface.body.body),
                    config,
                    false,
                    ctx,
                );
            }
            AstKind::TSTypeLiteral(type_literal) => {
                let config = self.type_literals.as_ref().unwrap_or(&self.default);
                validate_members_order(&type_literal_members(type_literal), config, false, ctx);
            }
            _ => {}
        }
    }

    fn should_run(&self, ctx: &ContextHost) -> bool {
        ctx.source_type().is_typescript()
    }
}

#[derive(Debug)]
struct MemberInfo<'a> {
    span: Span,
    name: Option<Cow<'a, str>>,
    optional: bool,
    candidates: Vec<String>,
    ignored_for_rank: bool,
}

fn validate_members_order<'a>(
    members: &[MemberInfo<'a>],
    order_config: &OrderConfig,
    supports_modifiers: bool,
    ctx: &LintContext<'a>,
) {
    let normalized = match NormalizedOrderConfig::new(order_config) {
        None => return,
        Some(normalized) => normalized,
    };

    if let Some(optionality_order) = normalized.optionality_order {
        let switch_index = members
            .iter()
            .enumerate()
            .find(|(index, member)| *index > 0 && member.optional != members[index - 1].optional)
            .map(|(index, _)| index);

        if let Some(switch_index) = switch_index {
            if !check_required_order(members, switch_index, optionality_order, ctx) {
                return;
            }
            check_order(&members[..switch_index], &normalized, supports_modifiers, ctx);
            check_order(&members[switch_index..], &normalized, supports_modifiers, ctx);
            return;
        }
    }

    check_order(members, &normalized, supports_modifiers, ctx);
}

struct NormalizedOrderConfig<'a> {
    member_types: Option<&'a [MemberTypeGroup]>,
    optionality_order: Option<OptionalityOrder>,
    order: Order,
}

impl<'a> NormalizedOrderConfig<'a> {
    fn new(config: &'a OrderConfig) -> Option<Self> {
        match config {
            OrderConfig::Never(_) => None,
            OrderConfig::MemberTypes(member_types) => Some(Self {
                member_types: Some(member_types),
                optionality_order: None,
                order: Order::AsWritten,
            }),
            OrderConfig::Sorted(sorted) => {
                let member_types = match &sorted.member_types {
                    Some(MemberTypesOption::Never(_)) => None,
                    Some(MemberTypesOption::Types(member_types)) => Some(member_types.as_slice()),
                    None => Some(default_member_types_static()),
                };
                Some(Self {
                    member_types,
                    optionality_order: sorted.optionality_order,
                    order: sorted.order,
                })
            }
        }
    }
}

fn check_order<'a>(
    members: &[MemberInfo<'a>],
    config: &NormalizedOrderConfig<'_>,
    supports_modifiers: bool,
    ctx: &LintContext<'a>,
) -> bool {
    if let Some(member_types) = config.member_types {
        let Some(groups) = check_group_sort(members, member_types, supports_modifiers, ctx) else {
            if config.order.is_sorted() {
                for group in group_members_by_type(members, member_types, supports_modifiers) {
                    check_alpha_sort(&group, config.order, ctx);
                }
            }
            return false;
        };

        if config.order.is_sorted() {
            for group in groups {
                check_alpha_sort(&group, config.order, ctx);
            }
        }
    } else if config.order.is_sorted() {
        let members = members.iter().collect::<Vec<_>>();
        return check_alpha_sort(&members, config.order, ctx);
    }

    false
}

fn check_group_sort<'a, 'b>(
    members: &'b [MemberInfo<'a>],
    member_types: &[MemberTypeGroup],
    supports_modifiers: bool,
    ctx: &LintContext<'a>,
) -> Option<Vec<Vec<&'b MemberInfo<'a>>>> {
    let mut previous_ranks = Vec::new();
    let mut member_groups: Vec<Vec<&'b MemberInfo<'a>>> = Vec::new();
    let mut correctly_sorted = true;

    for member in members {
        let rank = get_rank(member, member_types, supports_modifiers);
        if rank == -1 {
            continue;
        }

        let rank = rank as usize;
        if let Some(&last_rank) = previous_ranks.last() {
            if rank < last_rank {
                let name = member.name.as_deref().unwrap_or("");
                let rank_name = get_lowest_rank(&previous_ranks, rank, member_types);
                ctx.diagnostic(incorrect_group_order_diagnostic(member.span, name, &rank_name));
                correctly_sorted = false;
            } else if rank == last_rank {
                if let Some(last_group) = member_groups.last_mut() {
                    last_group.push(member);
                }
            } else {
                previous_ranks.push(rank);
                member_groups.push(vec![member]);
            }
        } else {
            previous_ranks.push(rank);
            member_groups.push(vec![member]);
        }
    }

    correctly_sorted.then_some(member_groups)
}

fn group_members_by_type<'a, 'b>(
    members: &'b [MemberInfo<'a>],
    member_types: &[MemberTypeGroup],
    supports_modifiers: bool,
) -> Vec<Vec<&'b MemberInfo<'a>>> {
    let ranks = members
        .iter()
        .map(|member| get_rank(member, member_types, supports_modifiers))
        .collect::<Vec<_>>();
    let mut groups: Vec<Vec<&'b MemberInfo<'a>>> = Vec::new();
    let mut previous_rank = None;

    for (index, member) in members.iter().enumerate() {
        if index == members.len().saturating_sub(1) {
            break;
        }
        let current_rank = ranks[index];
        let next_rank = ranks[index + 1];
        if Some(current_rank) == previous_rank {
            if let Some(last_group) = groups.last_mut() {
                last_group.push(member);
            }
        } else if current_rank == next_rank {
            groups.push(vec![member]);
            previous_rank = Some(current_rank);
        }
    }

    groups
}

fn check_alpha_sort<'a>(members: &[&MemberInfo<'a>], order: Order, ctx: &LintContext<'a>) -> bool {
    let mut previous_name = Cow::Borrowed("");
    let mut correctly_sorted = true;

    for member in members {
        let Some(name) = member.name.as_ref() else { continue };

        if natural_out_of_order(name, &previous_name, order) {
            ctx.diagnostic(incorrect_order_diagnostic(member.span, name, &previous_name));
            correctly_sorted = false;
        }

        previous_name = Cow::Owned(name.to_string());
    }

    correctly_sorted
}

fn check_required_order<'a>(
    members: &[MemberInfo<'a>],
    switch_index: usize,
    optionality_order: OptionalityOrder,
    ctx: &LintContext<'a>,
) -> bool {
    let expected_first_optional = matches!(optionality_order, OptionalityOrder::OptionalFirst);

    if members[0].optional != expected_first_optional {
        let optional_or_required = if matches!(optionality_order, OptionalityOrder::RequiredFirst) {
            "required"
        } else {
            "optional"
        };
        let name = members[0].name.as_deref().unwrap_or("");
        ctx.diagnostic(incorrect_required_members_order_diagnostic(
            members[0].span,
            name,
            optional_or_required,
        ));
        return false;
    }

    for index in switch_index + 1..members.len() {
        if members[index].optional != members[switch_index].optional {
            let optional_or_required =
                if matches!(optionality_order, OptionalityOrder::RequiredFirst) {
                    "required"
                } else {
                    "optional"
                };
            let member = &members[switch_index];
            let name = member.name.as_deref().unwrap_or("");
            ctx.diagnostic(incorrect_required_members_order_diagnostic(
                member.span,
                name,
                optional_or_required,
            ));
            return false;
        }
    }

    true
}

fn get_rank(
    member: &MemberInfo<'_>,
    member_types: &[MemberTypeGroup],
    _supports_modifiers: bool,
) -> isize {
    if member.ignored_for_rank {
        return -1;
    }

    for candidate in &member.candidates {
        if let Some(index) = member_types.iter().position(|group| group.contains(candidate)) {
            return index as isize;
        }
    }

    -1
}

fn get_lowest_rank(ranks: &[usize], target: usize, order: &[MemberTypeGroup]) -> String {
    let mut lowest = *ranks.last().unwrap_or(&target);
    for &rank in ranks {
        if rank > target {
            lowest = lowest.min(rank);
        }
    }
    order[lowest].display()
}

fn natural_out_of_order(name: &str, previous_name: &str, order: Order) -> bool {
    if name == previous_name {
        return false;
    }

    match order {
        Order::Alphabetically => name < previous_name,
        Order::AlphabeticallyCaseInsensitive => name.to_lowercase() < previous_name.to_lowercase(),
        Order::AsWritten => false,
        Order::Natural => natord::compare(name, previous_name) != std::cmp::Ordering::Greater,
        Order::NaturalCaseInsensitive => {
            natord::compare(&name.to_lowercase(), &previous_name.to_lowercase())
                != std::cmp::Ordering::Greater
        }
    }
}

fn class_members<'a>(class: &'a Class<'a>) -> Vec<MemberInfo<'a>> {
    class.body.body.iter().map(class_member_info).collect()
}

fn class_member_info<'a>(element: &'a ClassElement<'a>) -> MemberInfo<'a> {
    match element {
        ClassElement::StaticBlock(block) => MemberInfo {
            span: block.span,
            name: Some(Cow::Borrowed("static block")),
            optional: false,
            candidates: vec!["static-initialization".to_string()],
            ignored_for_rank: false,
        },
        ClassElement::MethodDefinition(method) => method_info(method),
        ClassElement::PropertyDefinition(property) => property_info(property),
        ClassElement::AccessorProperty(accessor) => accessor_info(accessor),
        ClassElement::TSIndexSignature(signature) => index_signature_info(signature),
    }
}

fn method_info<'a>(method: &'a MethodDefinition<'a>) -> MemberInfo<'a> {
    let kind = match method.kind {
        MethodDefinitionKind::Constructor => "constructor",
        MethodDefinitionKind::Method => "method",
        MethodDefinitionKind::Get => "get",
        MethodDefinitionKind::Set => "set",
    };
    let name = if method.kind == MethodDefinitionKind::Constructor {
        Some(Cow::Borrowed("constructor"))
    } else {
        member_name(&method.key)
    };

    MemberInfo {
        span: method.span,
        name,
        optional: method.optional,
        candidates: member_candidates(
            kind,
            method.accessibility,
            method.key.is_private_identifier(),
            method.r#static,
            method.r#type == MethodDefinitionType::TSAbstractMethodDefinition,
            !method.decorators.is_empty(),
        ),
        ignored_for_rank: method.value.body.is_none(),
    }
}

fn property_info<'a>(property: &'a PropertyDefinition<'a>) -> MemberInfo<'a> {
    let kind = if property.value.as_ref().is_some_and(is_function_expression) {
        "method"
    } else if property.readonly {
        "readonly-field"
    } else {
        "field"
    };

    MemberInfo {
        span: property.span,
        name: member_name(&property.key),
        optional: property.optional,
        candidates: member_candidates(
            kind,
            property.accessibility,
            property.key.is_private_identifier(),
            property.r#static,
            property.r#type == PropertyDefinitionType::TSAbstractPropertyDefinition,
            !property.decorators.is_empty(),
        ),
        ignored_for_rank: false,
    }
}

fn accessor_info<'a>(accessor: &'a AccessorProperty<'a>) -> MemberInfo<'a> {
    MemberInfo {
        span: accessor.span,
        name: member_name(&accessor.key),
        optional: false,
        candidates: member_candidates(
            "accessor",
            accessor.accessibility,
            accessor.key.is_private_identifier(),
            accessor.r#static,
            accessor.r#type == AccessorPropertyType::TSAbstractAccessorProperty,
            !accessor.decorators.is_empty(),
        ),
        ignored_for_rank: false,
    }
}

fn index_signature_info<'a>(signature: &'a TSIndexSignature<'a>) -> MemberInfo<'a> {
    let kind = if signature.readonly { "readonly-signature" } else { "signature" };
    let mut candidates = vec![kind.to_string()];
    if kind == "readonly-signature" {
        candidates.push("signature".to_string());
    }

    MemberInfo {
        span: signature.span,
        name: signature.parameters.first().map(|parameter| Cow::Borrowed(parameter.name.as_str())),
        optional: false,
        candidates,
        ignored_for_rank: false,
    }
}

fn type_literal_members<'a>(type_literal: &'a TSTypeLiteral<'a>) -> Vec<MemberInfo<'a>> {
    signature_members(&type_literal.members)
}

fn signature_members<'a>(signatures: &'a [TSSignature<'a>]) -> Vec<MemberInfo<'a>> {
    signatures.iter().map(signature_info).collect()
}

fn signature_info<'a>(signature: &'a TSSignature<'a>) -> MemberInfo<'a> {
    match signature {
        TSSignature::TSIndexSignature(signature) => index_signature_info(signature),
        TSSignature::TSPropertySignature(property) => {
            let kind = if property.readonly { "readonly-field" } else { "field" };
            let mut candidates = vec![kind.to_string()];
            if kind == "readonly-field" {
                candidates.push("field".to_string());
            }
            MemberInfo {
                span: property.span,
                name: member_name(&property.key),
                optional: property.optional,
                candidates,
                ignored_for_rank: false,
            }
        }
        TSSignature::TSCallSignatureDeclaration(signature) => MemberInfo {
            span: signature.span,
            name: Some(Cow::Borrowed("call")),
            optional: false,
            candidates: vec!["call-signature".to_string()],
            ignored_for_rank: false,
        },
        TSSignature::TSConstructSignatureDeclaration(signature) => MemberInfo {
            span: signature.span,
            name: Some(Cow::Borrowed("new")),
            optional: false,
            candidates: vec!["constructor".to_string()],
            ignored_for_rank: false,
        },
        TSSignature::TSMethodSignature(method) => {
            let kind = match method.kind {
                TSMethodSignatureKind::Method => "method",
                TSMethodSignatureKind::Get => "get",
                TSMethodSignatureKind::Set => "set",
            };
            MemberInfo {
                span: method.span,
                name: member_name(&method.key),
                optional: method.optional,
                candidates: vec![kind.to_string()],
                ignored_for_rank: false,
            }
        }
    }
}

fn member_candidates(
    kind: &str,
    accessibility: Option<TSAccessibility>,
    private_identifier: bool,
    is_static: bool,
    is_abstract: bool,
    decorated: bool,
) -> Vec<String> {
    let accessibility = accessibility_name(accessibility, private_identifier);
    let scope = if is_static {
        "static"
    } else if is_abstract {
        "abstract"
    } else {
        "instance"
    };
    let mut candidates = Vec::new();

    if decorated
        && matches!(kind, "readonly-field" | "field" | "method" | "accessor" | "get" | "set")
    {
        candidates.push(format!("{accessibility}-decorated-{kind}"));
        candidates.push(format!("decorated-{kind}"));
        if kind == "readonly-field" {
            candidates.push(format!("{accessibility}-decorated-field"));
            candidates.push("decorated-field".to_string());
        }
    }

    if !matches!(kind, "readonly-signature" | "signature" | "static-initialization") {
        if kind != "constructor" {
            candidates.push(format!("{accessibility}-{scope}-{kind}"));
            candidates.push(format!("{scope}-{kind}"));
            if kind == "readonly-field" {
                candidates.push(format!("{accessibility}-{scope}-field"));
                candidates.push(format!("{scope}-field"));
            }
        }

        candidates.push(format!("{accessibility}-{kind}"));
        if kind == "readonly-field" {
            candidates.push(format!("{accessibility}-field"));
        }
    }

    candidates.push(kind.to_string());
    if kind == "readonly-signature" {
        candidates.push("signature".to_string());
    } else if kind == "readonly-field" {
        candidates.push("field".to_string());
    }

    candidates
}

fn accessibility_name(
    accessibility: Option<TSAccessibility>,
    private_identifier: bool,
) -> &'static str {
    if private_identifier {
        return "#private";
    }

    match accessibility {
        Some(TSAccessibility::Private) => "private",
        Some(TSAccessibility::Protected) => "protected",
        Some(TSAccessibility::Public) | None => "public",
    }
}

fn member_name<'a>(key: &'a PropertyKey<'a>) -> Option<Cow<'a, str>> {
    key.name()
}

fn is_function_expression(expression: &Expression<'_>) -> bool {
    matches!(expression, Expression::FunctionExpression(_) | Expression::ArrowFunctionExpression(_))
}

static DEFAULT_MEMBER_TYPES: LazyLock<Vec<MemberTypeGroup>> = LazyLock::new(default_member_types);

fn default_member_types_static() -> &'static [MemberTypeGroup] {
    &DEFAULT_MEMBER_TYPES
}

fn default_member_types() -> Vec<MemberTypeGroup> {
    DEFAULT_ORDER
        .iter()
        .map(|name| MemberTypeGroup::Single(MemberTypeName((*name).to_string())))
        .collect()
}

static ALL_MEMBER_TYPES: LazyLock<FxHashSet<String>> = LazyLock::new(|| {
    let mut set = FxHashSet::default();
    let types = [
        "readonly-signature",
        "signature",
        "readonly-field",
        "field",
        "method",
        "call-signature",
        "constructor",
        "accessor",
        "get",
        "set",
        "static-initialization",
    ];
    let accessibilities = ["public", "protected", "private", "#private"];

    for ty in types {
        set.insert(ty.to_string());
        for accessibility in accessibilities {
            if !matches!(
                ty,
                "readonly-signature" | "signature" | "static-initialization" | "call-signature"
            ) && !(ty == "constructor" && accessibility == "#private")
            {
                set.insert(format!("{accessibility}-{ty}"));
            }

            if accessibility != "#private"
                && matches!(ty, "readonly-field" | "field" | "method" | "accessor" | "get" | "set")
            {
                set.insert(format!("{accessibility}-decorated-{ty}"));
                set.insert(format!("decorated-{ty}"));
            }

            if !matches!(ty, "constructor" | "readonly-signature" | "signature" | "call-signature")
            {
                let scopes: &[&str] = if matches!(accessibility, "#private" | "private") {
                    &["static", "instance"]
                } else {
                    &["static", "instance", "abstract"]
                };
                for scope in scopes {
                    set.insert(format!("{scope}-{ty}"));
                    set.insert(format!("{accessibility}-{scope}-{ty}"));
                }
            }
        }
    }

    set
});

fn all_member_types() -> &'static FxHashSet<String> {
    &ALL_MEMBER_TYPES
}

const DEFAULT_ORDER: &[&str] = &[
    "signature",
    "call-signature",
    "public-static-field",
    "protected-static-field",
    "private-static-field",
    "#private-static-field",
    "public-decorated-field",
    "protected-decorated-field",
    "private-decorated-field",
    "public-instance-field",
    "protected-instance-field",
    "private-instance-field",
    "#private-instance-field",
    "public-abstract-field",
    "protected-abstract-field",
    "public-field",
    "protected-field",
    "private-field",
    "#private-field",
    "static-field",
    "instance-field",
    "abstract-field",
    "decorated-field",
    "field",
    "static-initialization",
    "public-constructor",
    "protected-constructor",
    "private-constructor",
    "constructor",
    "public-static-accessor",
    "protected-static-accessor",
    "private-static-accessor",
    "#private-static-accessor",
    "public-decorated-accessor",
    "protected-decorated-accessor",
    "private-decorated-accessor",
    "public-instance-accessor",
    "protected-instance-accessor",
    "private-instance-accessor",
    "#private-instance-accessor",
    "public-abstract-accessor",
    "protected-abstract-accessor",
    "public-accessor",
    "protected-accessor",
    "private-accessor",
    "#private-accessor",
    "static-accessor",
    "instance-accessor",
    "abstract-accessor",
    "decorated-accessor",
    "accessor",
    "public-static-get",
    "protected-static-get",
    "private-static-get",
    "#private-static-get",
    "public-decorated-get",
    "protected-decorated-get",
    "private-decorated-get",
    "public-instance-get",
    "protected-instance-get",
    "private-instance-get",
    "#private-instance-get",
    "public-abstract-get",
    "protected-abstract-get",
    "public-get",
    "protected-get",
    "private-get",
    "#private-get",
    "static-get",
    "instance-get",
    "abstract-get",
    "decorated-get",
    "get",
    "public-static-set",
    "protected-static-set",
    "private-static-set",
    "#private-static-set",
    "public-decorated-set",
    "protected-decorated-set",
    "private-decorated-set",
    "public-instance-set",
    "protected-instance-set",
    "private-instance-set",
    "#private-instance-set",
    "public-abstract-set",
    "protected-abstract-set",
    "public-set",
    "protected-set",
    "private-set",
    "#private-set",
    "static-set",
    "instance-set",
    "abstract-set",
    "decorated-set",
    "set",
    "public-static-method",
    "protected-static-method",
    "private-static-method",
    "#private-static-method",
    "public-decorated-method",
    "protected-decorated-method",
    "private-decorated-method",
    "public-instance-method",
    "protected-instance-method",
    "private-instance-method",
    "#private-instance-method",
    "public-abstract-method",
    "protected-abstract-method",
    "public-method",
    "protected-method",
    "private-method",
    "#private-method",
    "static-method",
    "instance-method",
    "abstract-method",
    "decorated-method",
    "method",
];

#[test]
fn test() {
    use crate::tester::Tester;

    let pass = vec![
        (
            "interface Foo { [z: string]: unknown; a(): void; new (): Foo; b: string; }",
            Some(
                serde_json::json!([{ "default": ["signature", "method", "constructor", "field"] }]),
            ),
        ),
        (
            "type Foo = { [z: string]: unknown; a(): void; new (): Foo; b: string; };",
            Some(
                serde_json::json!([{ "typeLiterals": ["signature", "method", "constructor", "field"] }]),
            ),
        ),
        ("class Foo { public static a: string; private b: string; constructor() {} c() {} }", None),
        ("class Foo { b: string; a(): void; }", Some(serde_json::json!([{ "default": "never" }]))),
        (
            "interface Foo { a: string; b(): void; }",
            Some(
                serde_json::json!([{ "default": { "memberTypes": "never", "order": "alphabetically" } }]),
            ),
        ),
        (
            "interface Foo { a?: string; b: string; }",
            Some(serde_json::json!([{ "default": { "optionalityOrder": "optional-first" } }])),
        ),
    ];

    let fail = vec![
        (
            "interface Foo { b: string; new (): Foo; a(): void; [z: string]: unknown; }",
            Some(
                serde_json::json!([{ "default": ["signature", "method", "constructor", "field"] }]),
            ),
        ),
        (
            "type Foo = { b: string; a(): void; new (): Foo; [z: string]: unknown; };",
            Some(
                serde_json::json!([{ "typeLiterals": ["signature", "method", "constructor", "field"] }]),
            ),
        ),
        ("class Foo { c() {} constructor() {} b: string; }", None),
        (
            "class Foo { public static a() {} public static b: string; }",
            Some(
                serde_json::json!([{ "default": ["public-static-field", "public-static-method"] }]),
            ),
        ),
        (
            "interface Foo { b(): void; a: string; }",
            Some(
                serde_json::json!([{ "default": { "memberTypes": "never", "order": "alphabetically" } }]),
            ),
        ),
        (
            "interface Foo { a: string; b?: string; c: string; }",
            Some(serde_json::json!([{ "default": { "optionalityOrder": "required-first" } }])),
        ),
    ];

    Tester::new(MemberOrdering::NAME, MemberOrdering::PLUGIN, pass, fail).test_and_snapshot();
}
