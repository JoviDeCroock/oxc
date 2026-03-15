use std::path::Path;

use oxc_allocator::Allocator;
use oxc_codegen::{Codegen, CodegenOptions};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{JsxOptions, PrefreshOptions, TransformOptions, Transformer};

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

fn transform(source_text: &str, file_name: &str, prefresh: PrefreshOptions) -> String {
    let source_type = SourceType::tsx().with_module(true);
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, source_text, source_type).parse();
    assert!(ret.errors.is_empty(), "parse errors: {:?}", ret.errors);

    let mut program = ret.program;
    let scoping = SemanticBuilder::new().build(&program).semantic.into_scoping();

    let options = TransformOptions {
        jsx: JsxOptions { prefresh: Some(prefresh), ..JsxOptions::default() },
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
fn transforms_create_context_calls() {
    let output = transform(
        "import { createContext } from 'preact'; const Ctx = createContext(0);",
        "context.tsx",
        PrefreshOptions::default(),
    );
    let hash = hash_base36("context.tsx");

    assert!(output.contains("Object.assign("), "{output}");
    assert!(output.contains(&format!("createContext['_{}$Ctx']", hash)), "{output}");
    assert!(output.contains("{ __: 0 }"), "{output}");
}

#[test]
fn create_context_ids_capture_arrow_params() {
    let output = transform(
        "import { createContext } from 'preact'; const make = foo => createContext(foo);",
        "closure.tsx",
        PrefreshOptions::default(),
    );

    assert!(output.contains("createContext[`"), "{output}");
    assert!(output.contains("${foo}`"), "{output}");
}

#[test]
fn registers_class_components() {
    let output = transform("class App {}", "class-component.tsx", PrefreshOptions::default());

    assert!(output.contains("_c = App;"), "{output}");
    assert!(output.contains("$RefreshReg$(_c, 'App');"), "{output}");
}

#[test]
fn treats_use_signal_as_builtin_hook() {
    let output = transform(
        "const App = () => { const value = useSignal(0); return value; };",
        "hooks.tsx",
        PrefreshOptions { emit_full_signatures: true, ..PrefreshOptions::default() },
    );

    assert!(output.contains("_s(App, 'useSignal{value(0)}')"), "{output}");
}
