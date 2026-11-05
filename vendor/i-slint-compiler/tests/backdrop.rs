use i_slint_compiler::diagnostics::BuildDiagnostics;
use i_slint_compiler::generator::OutputFormat;
use i_slint_compiler::{CompilerConfiguration, compile_syntax_node, parser};

#[test]
fn backdrop_is_public_and_accepts_children_and_all_effect_properties() {
    let mut diagnostics = BuildDiagnostics::default();
    let syntax = parser::parse(
        r#"
        export component Test inherits Window {
            width: 200px; height: 150px;
            Backdrop {
                x: 5px; y: 10px; width: 180px; height: 130px;
                blur: 12px; saturation: 1.7; brightness: -0.1;
                refraction: 0.5; border-radius: 24px;
                Text { text: "Sharp foreground"; }
            }
        }
    "#
        .into(),
        None,
        &mut diagnostics,
    );
    let (_, diagnostics, _) = spin_on::spin_on(compile_syntax_node(
        syntax,
        diagnostics,
        CompilerConfiguration::new(OutputFormat::Interpreter),
    ));
    assert!(
        !diagnostics.has_errors(),
        "{:?}",
        diagnostics.to_string_vec()
    );
}

fn compile(source: &str) -> BuildDiagnostics {
    let mut diagnostics = BuildDiagnostics::default();
    let syntax = parser::parse(source.into(), None, &mut diagnostics);
    let mut config = CompilerConfiguration::new(OutputFormat::Interpreter);
    config.enable_experimental = true;
    let (_, diagnostics, _) =
        spin_on::spin_on(compile_syntax_node(syntax, diagnostics, config));
    diagnostics
}

#[test]
fn patched_text_and_flickable_members_compile() {
    let diagnostics = compile(r#"
        export component Test inherits Window {
            property <string> destination;
            property <string> visible-label;
            property <length> pointer-x;
            property <length> pointer-y;
            Flickable {
                immediate-child-press: true;
                content-width: 300px;
                content-height: 400px;
                StyledText {
                    text: @markdown("[Visible label](https://example.com)");
                    link-clicked(url, label) => {
                        root.destination = url;
                        root.visible-label = label;
                    }
                    pointer-event(event) => {
                        root.pointer-x = self.mouse-x;
                        root.pointer-y = self.mouse-y;
                    }
                }
            }
            input := TextInput { text: "caret"; }
            Rectangle {
                x: input.visual-cursor-x;
                y: input.visual-cursor-y;
                width: input.visual-cursor-width;
                height: input.visual-cursor-height;
            }
        }
    "#);
    assert!(!diagnostics.has_errors(), "{:?}", diagnostics.to_string_vec());
}

#[test]
fn patched_native_geometry_remains_read_only() {
    for body in [
        "TextInput { visual-cursor-x: 1px; }",
        "TextInput { visual-cursor-y: 1px; }",
        "TextInput { visual-cursor-width: 1px; }",
        "TextInput { visual-cursor-height: 1px; }",
        "StyledText { mouse-x: 1px; }",
        "StyledText { mouse-y: 1px; }",
    ] {
        let diagnostics = compile(&format!("export component Test inherits Window {{ {body} }}"));
        assert!(diagnostics.has_errors(), "output property became writable: {body}");
    }
}

#[test]
fn backdrop_is_a_public_parent_sized_native_item() {
    use i_slint_compiler::langtype::{DefaultSizeBinding, ElementType};
    let registry = i_slint_compiler::typeregister::TypeRegister::builtin();
    let ElementType::Builtin(backdrop) = registry.borrow().lookup_element("Backdrop").unwrap() else {
        panic!("Backdrop must be a builtin item");
    };
    assert!(!backdrop.is_internal);
    for (name, expected) in [("saturation", 1.0), ("brightness", 0.0), ("refraction", 0.0)] {
        let expression = backdrop.properties[name].default_value.expr_without_element().unwrap();
        assert!(matches!(expression,
            i_slint_compiler::expression_tree::Expression::NumberLiteral(value, _) if value == expected
        ), "wrong Backdrop default for {name}");
    }
    assert_eq!(backdrop.native_class.class_name, "Backdrop");
    assert_eq!(backdrop.default_size_binding, DefaultSizeBinding::ExpandsToParentGeometry);
}
