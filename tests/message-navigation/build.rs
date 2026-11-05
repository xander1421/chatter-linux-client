use std::{env, fs, path::PathBuf};
fn main() {
    let source = PathBuf::from("../../ui/chat/message_list.slint")
        .canonicalize()
        .unwrap();
    println!("cargo:rerun-if-changed={}", source.display());
    let text = fs::read_to_string(&source).unwrap();
    // Compile the production component verbatim; expose internal state solely
    // in the generated test copy. No navigation implementation lives here.
    let text = text
        .replace("private property", "in-out property")
        .replace("    function go-to-row(", "    public function go-to-row(")
        .replace(
            "    function stop-row-seek(",
            "    public function stop-row-seek(",
        );
    let text = text.replace("export component MessageList inherits Rectangle {", "export component MessageList inherits Rectangle {\n    out property <bool> test-deadline-running: row-seek-deadline.running;");
    let text = text
        .lines()
        .map(|line| {
            if line.starts_with("import ") {
                let (prefix, rest) = line.split_once('"').unwrap();
                let (path, suffix) = rest.split_once('"').unwrap();
                format!(
                    "{prefix}\"{}\"{suffix}",
                    source.parent().unwrap().join(path).display()
                )
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::write(out.join("message_list.slint"), text).unwrap();
    fs::write(
        out.join("probe.slint"),
        r#"
        import { MessageList, MessageItemData } from "message_list.slint";
        export { MessageItemData }
        export component Probe inherits Window {
            width: 400px; height: 300px;
            in property <[MessageItemData]> messages <=> list.messages;
            in-out property <bool> active <=> list.page-active;
            in-out property <bool> loading <=> list.loading;
            in-out property <bool> bottom <=> list.is-at-bottom;
            out property <int> target: list.jump-row;
            out property <bool> deadline: list.test-deadline-running;
            out property <length> bubble-y: list.jump-bubble-y;
            out property <int> stable: list.jump-stable-passes;
            out property <bool> measured: list.jump-measured;
            out property <length> position: list.scroll-position;
            in-out property <int> unread <=> list.unread-index;
            public function seek(i: int) { list.go-to-row(i); }
            public function cancel() { list.stop-row-seek(); }
            list := MessageList { width: 100%; height: 100%; }
        }
    "#,
    )
    .unwrap();
    slint_build::compile(out.join("probe.slint")).unwrap();
}
