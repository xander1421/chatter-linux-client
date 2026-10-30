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
