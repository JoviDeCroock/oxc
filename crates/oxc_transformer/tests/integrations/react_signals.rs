use std::path::Path;

use oxc_allocator::Allocator;
use oxc_codegen::{Codegen, CodegenOptions};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{
    JsxOptions, ReactSignalsExperimentalOptions, ReactSignalsMode, ReactSignalsOptions,
    TransformOptions, Transformer,
};

fn transform(
    source_text: &str,
    source_type: SourceType,
    file_name: &str,
    signals: ReactSignalsOptions,
) -> String {
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, source_text, source_type).parse();
    assert!(ret.errors.is_empty(), "parse errors: {:?}", ret.errors);

    let mut program = ret.program;
    let scoping = SemanticBuilder::new().build(&program).semantic.into_scoping();

    let options = TransformOptions {
        jsx: JsxOptions { signals: Some(signals), ..JsxOptions::disable() },
        ..TransformOptions::default()
    };

    let ret = Transformer::new(&allocator, Path::new(file_name), &options)
        .build_with_scoping(scoping, &mut program);
    assert!(ret.errors.is_empty(), "transform errors: {:?}", ret.errors);

    Codegen::new()
        .with_options(CodegenOptions { single_quote: true, ..CodegenOptions::default() })
        .build(&program)
        .code
}

#[test]
fn transforms_components_in_auto_mode() {
    let output = transform(
        "function MyComponent() { signal.value; return <div>Hello</div>; }",
        SourceType::jsx().with_module(true),
        "Component.jsx",
        ReactSignalsOptions::default(),
    );

    assert_eq!(
        output,
        "import { useSignals as _useSignals } from '@preact/signals-react/runtime';\nfunction MyComponent() {\n\tvar _effect = _useSignals(1);\n\ttry {\n\t\tsignal.value;\n\t\treturn <div>Hello</div>;\n\t} finally {\n\t\t_effect.f();\n\t}\n}\n"
    );
}

#[test]
fn transforms_custom_hooks_in_auto_mode() {
    let output = transform(
        "function useValue() { return signal.value; }",
        SourceType::mjs(),
        "useValue.js",
        ReactSignalsOptions::default(),
    );

    assert_eq!(
        output,
        "import { useSignals as _useSignals } from '@preact/signals-react/runtime';\nfunction useValue() {\n\tvar _effect = _useSignals(2);\n\ttry {\n\t\treturn signal.value;\n\t} finally {\n\t\t_effect.f();\n\t}\n}\n"
    );
}

#[test]
fn detects_signal_destructuring_in_auto_mode() {
    let output = transform(
        "function MyComponent() { const { value } = signal; return <div>{value}</div>; }",
        SourceType::jsx().with_module(true),
        "Component.jsx",
        ReactSignalsOptions::default(),
    );

    assert_eq!(
        output,
        "import { useSignals as _useSignals } from '@preact/signals-react/runtime';\nfunction MyComponent() {\n\tvar _effect = _useSignals(1);\n\ttry {\n\t\tconst { value } = signal;\n\t\treturn <div>{value}</div>;\n\t} finally {\n\t\t_effect.f();\n\t}\n}\n"
    );
}

#[test]
fn supports_manual_opt_in_for_unmanaged_functions() {
    let output = transform(
        "/* @useSignals */ function render() { return signal.value; }",
        SourceType::mjs(),
        "render.js",
        ReactSignalsOptions { mode: ReactSignalsMode::Manual, ..ReactSignalsOptions::default() },
    );

    assert_eq!(
        output,
        "import { useSignals as _useSignals } from '@preact/signals-react/runtime';\n/* @useSignals */ function render() {\n\t_useSignals();\n\treturn signal.value;\n}\n"
    );
}

#[test]
fn opt_out_comment_wins_over_opt_in() {
    let output = transform(
        "/** @noUseSignals @useSignals */ function MyComponent() { signal.value; return <div>Hello</div>; }",
        SourceType::jsx().with_module(true),
        "Component.jsx",
        ReactSignalsOptions::default(),
    );

    assert_eq!(
        output,
        "/** @noUseSignals @useSignals */ function MyComponent() {\n\tsignal.value;\n\treturn <div>Hello</div>;\n}\n"
    );
}

#[test]
fn transforms_components_without_signal_reads_in_all_mode() {
    let output = transform(
        "function MyComponent() { return <div>Hello</div>; }",
        SourceType::jsx().with_module(true),
        "Component.jsx",
        ReactSignalsOptions { mode: ReactSignalsMode::All, ..ReactSignalsOptions::default() },
    );

    assert_eq!(
        output,
        "import { useSignals as _useSignals } from '@preact/signals-react/runtime';\nfunction MyComponent() {\n\tvar _effect = _useSignals(1);\n\ttry {\n\t\treturn <div>Hello</div>;\n\t} finally {\n\t\t_effect.f();\n\t}\n}\n"
    );
}

#[test]
fn supports_no_try_finally_mode() {
    let output = transform(
        "function MyComponent() { signal.value; return <div>Hello</div>; }",
        SourceType::jsx().with_module(true),
        "Component.jsx",
        ReactSignalsOptions {
            experimental: ReactSignalsExperimentalOptions {
                no_try_finally: true,
                ..ReactSignalsExperimentalOptions::default()
            },
            ..ReactSignalsOptions::default()
        },
    );

    assert_eq!(
        output,
        "import { useSignals as _useSignals } from '@preact/signals-react/runtime';\nfunction MyComponent() {\n\t_useSignals();\n\tsignal.value;\n\treturn <div>Hello</div>;\n}\n"
    );
}

#[test]
fn supports_custom_import_source() {
    let output = transform(
        "function MyComponent() { signal.value; return <div>Hello</div>; }",
        SourceType::jsx().with_module(true),
        "Component.jsx",
        ReactSignalsOptions {
            import_source: Some(String::from("custom-source")),
            ..ReactSignalsOptions::default()
        },
    );

    assert_eq!(
        output,
        "import { useSignals as _useSignals } from 'custom-source';\nfunction MyComponent() {\n\tvar _effect = _useSignals(1);\n\ttry {\n\t\tsignal.value;\n\t\treturn <div>Hello</div>;\n\t} finally {\n\t\t_effect.f();\n\t}\n}\n"
    );
}

#[test]
fn detects_pretransformed_jsx_calls() {
    let output = transform(
        "import { jsx as _jsx } from 'react/jsx-runtime'; function MyComponent() { signal.value; return _jsx('div', { children: 'Hello' }); }",
        SourceType::mjs(),
        "Component.js",
        ReactSignalsOptions { detect_transformed_jsx: true, ..ReactSignalsOptions::default() },
    );

    assert_eq!(
        output,
        "import { jsx as _jsx } from 'react/jsx-runtime';\nimport { useSignals as _useSignals } from '@preact/signals-react/runtime';\nfunction MyComponent() {\n\tvar _effect = _useSignals(1);\n\ttry {\n\t\tsignal.value;\n\t\treturn _jsx('div', { children: 'Hello' });\n\t} finally {\n\t\t_effect.f();\n\t}\n}\n"
    );
}

#[test]
fn detects_react_create_element_calls() {
    let output = transform(
        "import React from 'react'; function MyComponent() { signal.value; return React.createElement('div', null, 'Hello'); }",
        SourceType::mjs(),
        "Component.js",
        ReactSignalsOptions { detect_transformed_jsx: true, ..ReactSignalsOptions::default() },
    );

    assert_eq!(
        output,
        "import React from 'react';\nimport { useSignals as _useSignals } from '@preact/signals-react/runtime';\nfunction MyComponent() {\n\tvar _effect = _useSignals(1);\n\ttry {\n\t\tsignal.value;\n\t\treturn React.createElement('div', null, 'Hello');\n\t} finally {\n\t\t_effect.f();\n\t}\n}\n"
    );
}

#[test]
fn adds_debug_metadata_for_signals() {
    let output = transform(
        "function MyComponent() { const count = signal(0); return <div>{count.value}</div>; }",
        SourceType::jsx().with_module(true),
        "Component.jsx",
        ReactSignalsOptions {
            experimental: ReactSignalsExperimentalOptions {
                debug: true,
                ..ReactSignalsExperimentalOptions::default()
            },
            ..ReactSignalsOptions::default()
        },
    );

    assert!(output.contains("var _effect = _useSignals(1, 'MyComponent');"), "{output}");
    assert!(
        output.contains("const count = signal(0, { name: 'count (Component.jsx:1)' });"),
        "{output}"
    );
}

#[test]
fn uses_require_in_commonjs_files() {
    let output = transform(
        "function MyComponent() { signal.value; return <div>Hello</div>; }",
        SourceType::jsx().with_commonjs(true),
        "Component.cjs",
        ReactSignalsOptions::default(),
    );

    assert_eq!(
        output,
        "var _useSignals = require('@preact/signals-react/runtime').useSignals;\nfunction MyComponent() {\n\tvar _effect = _useSignals(1);\n\ttry {\n\t\tsignal.value;\n\t\treturn <div>Hello</div>;\n\t} finally {\n\t\t_effect.f();\n\t}\n}\n"
    );
}
