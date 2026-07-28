//! The one check that catches the dominant failure mode of this crate.
//!
//! Every other error in the Rust↔Kotlin bridge fails at compile time. A JNI
//! descriptor mismatch does not. A wrong descriptor, a renamed class, or a moved
//! package produces `NoSuchMethodError` at *first call* — on a user's phone, in the
//! field, with the mesh silently dead and no crash until a peer happens to appear.
//!
//! This test runs `javap -s -p` over the Kotlin class Gradle compiled and asserts,
//! byte for byte, that every descriptor in [`BRIDGE_METHODS`] and [`NATIVE_METHODS`]
//! is present. It needs no device and no emulator.
//!
//! The contract is imported from the crate, never copied, so the test cannot drift
//! away from what `RegisterNatives` actually registers.
//!
//! # Skipping
//!
//! On a machine that has never run Gradle there is no class to inspect, so the check
//! skips rather than fails — but it says loudly what to run to make it real. A
//! silently-skipping check is worse than no check, because it reads as a passing one.
//! Set `CHATTER_JNI_SIGNATURES_REQUIRED=1` (do this in CI) to turn every skip into a
//! hard failure. Skip reasons are printed, so run with `-- --nocapture` to see them.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use chatter_mesh_android::{BRIDGE_CLASS, BRIDGE_METHODS, NATIVE_METHODS};

/// Set in CI. Turns "class not built" / "javap missing" from a skip into a failure.
const REQUIRED_ENV: &str = "CHATTER_JNI_SIGNATURES_REQUIRED";
/// Points at the Gradle build output when it is not at the default location.
const BUILD_DIR_ENV: &str = "CHATTER_ANDROID_BUILD_DIR";

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

#[test]
fn compiled_kotlin_matches_the_jni_contract() {
    let build_dir = build_dir();

    let Some(javap) = find_javap() else {
        skip(
            "`javap` was not found on PATH and JAVA_HOME is unset or has no bin/javap.",
            "Install a JDK (any version ≥ 8; `javap` ships with it), then re-run.",
        );
        return;
    };

    let class_file = match find_bridge_class(&build_dir) {
        Ok(path) => path,
        Err(reason) => {
            skip(
                &reason,
                &format!(
                    "Build the Kotlin first:\n    \
                     cd {}\n    ./gradlew :app:compileDebugKotlin\n  \
                     then re-run this test.",
                    build_dir
                        .parent()
                        .and_then(|p| p.parent())
                        .unwrap_or(&build_dir)
                        .display()
                ),
            );
            return;
        }
    };

    if let Some(source) = bridge_source(&build_dir) {
        if is_newer(&source, &class_file) {
            skip(
                &format!(
                    "{} is newer than the compiled class {} — the class on disk is \
                     stale, so checking it would prove nothing.",
                    source.display(),
                    class_file.display()
                ),
                "Re-run `./gradlew :app:compileDebugKotlin`, then re-run this test.",
            );
            return;
        }
    }

    let output = Command::new(&javap)
        .arg("-s")
        .arg("-p")
        .arg(&class_file)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {} on {}: {e}", javap.display(), class_file.display()));

    assert!(
        output.status.success(),
        "javap failed on {}:\n{}",
        class_file.display(),
        String::from_utf8_lossy(&output.stderr)
    );

    let text = String::from_utf8_lossy(&output.stdout);
    let parsed = parse_javap(&text);

    let problems = check(&parsed);
    assert!(
        problems.is_empty(),
        "\n\
         ===============================================================\n\
         JNI CONTRACT MISMATCH — this would be a NoSuchMethodError on a phone\n\
         ===============================================================\n\
         class file : {}\n\
         class      : {}\n\
         contract   : chatter-mesh-android/src/lib.rs (BRIDGE_METHODS / NATIVE_METHODS)\n\
         \n{}\n\
         ---------------------------------------------------------------\n\
         Fix the Kotlin, or change the contract in lib.rs — but they must agree\n\
         exactly. Nothing else in this crate catches this before a user does.\n",
        class_file.display(),
        parsed.class_name.as_deref().unwrap_or("<unparsed>"),
        problems.join("\n"),
    );
}

// ---------------------------------------------------------------------------
// Locating things
// ---------------------------------------------------------------------------

/// `linux-client/android/app/build`, unless overridden.
fn build_dir() -> PathBuf {
    if let Some(dir) = env::var_os(BUILD_DIR_ENV) {
        return PathBuf::from(dir);
    }
    // CARGO_MANIFEST_DIR = linux-client/crates/chatter-mesh-android
    normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../android/app/build"))
}

/// Resolve `..` textually, so the paths in failure messages are readable.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir if out.parent().is_some() => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// The package path the class must sit under, derived from the Rust constant so a
/// package move breaks here too: `com/chatter/client/MeshBleBridge.class`.
fn class_relative_path() -> PathBuf {
    let mut path = PathBuf::new();
    let (package, simple) = BRIDGE_CLASS.rsplit_once('/').unwrap_or(("", BRIDGE_CLASS));
    for segment in package.split('/').filter(|s| !s.is_empty()) {
        path.push(segment);
    }
    path.push(format!("{simple}.class"));
    path
}

/// The Kotlin source that produced the class, if the build dir is in its usual place.
fn bridge_source(build_dir: &Path) -> Option<PathBuf> {
    let app = build_dir.parent()?;
    let mut path = app.join("src/main/java");
    let (package, simple) = BRIDGE_CLASS.rsplit_once('/').unwrap_or(("", BRIDGE_CLASS));
    for segment in package.split('/').filter(|s| !s.is_empty()) {
        path.push(segment);
    }
    path.push(format!("{simple}.kt"));
    path.exists().then_some(path)
}

/// Newest `…/com/chatter/client/MeshBleBridge.class` under the build dir.
///
/// Gradle emits several copies (`tmp/kotlin-classes/<variant>`, `intermediates/…`);
/// the freshest one is the one that would ship. Nested classes (`MeshBleBridge$…`)
/// are deliberately *not* accepted as a substitute: a bridge method that ended up on
/// a companion or inner class is exactly the drift this test exists to catch.
fn find_bridge_class(build_dir: &Path) -> Result<PathBuf, String> {
    if !build_dir.is_dir() {
        return Err(format!(
            "no Gradle build output at {} — the Android app has never been built here.",
            build_dir.display()
        ));
    }

    let wanted = class_relative_path();
    let (_, simple) = BRIDGE_CLASS.rsplit_once('/').unwrap_or(("", BRIDGE_CLASS));
    let nested_prefix = format!("{simple}$");

    let mut exact: Vec<PathBuf> = Vec::new();
    let mut nested: Vec<PathBuf> = Vec::new();
    let mut walked = 0usize;
    walk(build_dir, &mut walked, &mut |file| {
        let Some(name) = file.file_name().and_then(|n| n.to_str()) else {
            return;
        };
        if !name.ends_with(".class") {
            return;
        }
        if file.ends_with(&wanted) {
            exact.push(file.to_path_buf());
        } else if name.starts_with(&nested_prefix) {
            nested.push(file.to_path_buf());
        }
    });

    if exact.is_empty() {
        if nested.is_empty() {
            return Err(format!(
                "no {} anywhere under {} ({walked} directories searched) — \
                 the Kotlin bridge has not been compiled.",
                wanted.display(),
                build_dir.display()
            ));
        }
        return Err(format!(
            "found nested classes ({}) but no {} under {} — the outer class is \
             missing from the build output.",
            nested
                .iter()
                .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
                .collect::<Vec<_>>()
                .join(", "),
            wanted.display(),
            build_dir.display()
        ));
    }

    exact.sort();
    let newest = exact
        .iter()
        .max_by_key(|p| mtime(p))
        .cloned()
        .expect("non-empty");
    Ok(newest)
}

/// Depth-first walk that does not follow symlinks (Gradle build dirs contain them).
fn walk(dir: &Path, counter: &mut usize, visit: &mut impl FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    *counter += 1;
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        let path = entry.path();
        if kind.is_dir() {
            walk(&path, counter, visit);
        } else if kind.is_file() {
            visit(&path);
        }
    }
}

fn mtime(path: &Path) -> SystemTime {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn is_newer(a: &Path, b: &Path) -> bool {
    mtime(a) > mtime(b)
}

/// `javap` from PATH, or from `JAVA_HOME/bin` when PATH does not have it.
fn find_javap() -> Option<PathBuf> {
    if let Some(home) = env::var_os("JAVA_HOME") {
        let candidate = Path::new(&home).join("bin").join("javap");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    Command::new("javap")
        .arg("-version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| PathBuf::from("javap"))
}

// ---------------------------------------------------------------------------
// Parsing `javap -s -p`
// ---------------------------------------------------------------------------

/// One member as `javap -s -p` prints it: a declaration line, then `descriptor:`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Member {
    name: String,
    descriptor: String,
    modifiers: Vec<String>,
}

impl Member {
    fn is_native(&self) -> bool {
        self.modifiers.iter().any(|m| m == "native")
    }

    fn render(&self) -> String {
        format!("{}{}  [{}]", self.name, self.descriptor, self.modifiers.join(" "))
    }
}

#[derive(Debug, Default)]
struct Javap {
    class_name: Option<String>,
    methods: Vec<Member>,
}

impl Javap {
    fn by_name(&self, name: &str) -> Vec<&Member> {
        self.methods.iter().filter(|m| m.name == name).collect()
    }
}

/// Parse `javap -s -p` output into methods.
///
/// The shape is two lines per member:
///
/// ```text
///   public boolean send(java.lang.String, byte[]);
///     descriptor: (Ljava/lang/String;[B)Z
/// ```
///
/// Fields use the same shape, so methods are told apart by the descriptor starting
/// with `(` — which also drops `static {}` blocks, whose declaration has no
/// parameter list to take a name from. Kotlin's synthetic members (`foo$lambda$3`,
/// `setScanning$default`, `access$getFoo$p`) parse as ordinary methods and are simply
/// never matched, since lookup is by exact name.
fn parse_javap(output: &str) -> Javap {
    let mut parsed = Javap::default();
    let mut pending: Option<&str> = None;

    for raw in output.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("descriptor:") {
            let descriptor = rest.trim();
            if let Some(decl) = pending.take() {
                if descriptor.starts_with('(') {
                    if let Some(member) = parse_declaration(decl, descriptor) {
                        parsed.methods.push(member);
                    }
                }
            }
            continue;
        }
        if parsed.class_name.is_none() {
            if let Some(name) = parse_class_header(line) {
                parsed.class_name = Some(name);
                pending = None;
                continue;
            }
        }
        pending = Some(line);
    }

    parsed
}

/// `public final class com.chatter.client.MeshBleBridge extends … {` → the class name.
fn parse_class_header(line: &str) -> Option<String> {
    if !line.ends_with('{') {
        return None;
    }
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let keyword = tokens
        .iter()
        .position(|t| matches!(*t, "class" | "interface" | "enum" | "record"))?;
    let name = tokens.get(keyword + 1)?;
    // Strip a generic parameter list: `Foo<T>` → `Foo`.
    Some(name.split('<').next().unwrap_or(name).to_string())
}

/// `private final native void nativeOnData(java.lang.String, byte[]);` → member.
fn parse_declaration(decl: &str, descriptor: &str) -> Option<Member> {
    let decl = decl.trim().trim_end_matches(';').trim();
    // Everything before the parameter list: modifiers, return type, name.
    let open = decl.find('(')?;
    let head = &decl[..open];
    let mut tokens: Vec<&str> = head.split_whitespace().collect();
    let name = tokens.pop()?;
    // Constructors print fully qualified (`public com.chatter.client.MeshBleBridge()`).
    let name = if name.contains('.') {
        "<init>".to_string()
    } else {
        name.to_string()
    };
    Some(Member {
        name,
        descriptor: descriptor.to_string(),
        modifiers: tokens.into_iter().map(str::to_string).collect(),
    })
}

// ---------------------------------------------------------------------------
// The contract check
// ---------------------------------------------------------------------------

/// Compare parsed `javap` output against the Rust contract. Returns every problem,
/// not just the first — a descriptor drift usually comes with company.
fn check(parsed: &Javap) -> Vec<String> {
    let mut problems = Vec::new();

    let expected_class = BRIDGE_CLASS.replace('/', ".");
    match parsed.class_name.as_deref() {
        Some(found) if found == expected_class => {}
        Some(found) => problems.push(format!(
            "  class name: expected `{expected_class}` (BRIDGE_CLASS), javap reported `{found}`.\n\
             \x20     FindClass would throw ClassNotFoundException at load."
        )),
        None => problems.push(
            "  class name: could not parse a class header out of the javap output.".to_string(),
        ),
    }

    for (label, table, must_be_native) in [
        ("BRIDGE_METHODS", BRIDGE_METHODS, false),
        ("NATIVE_METHODS", NATIVE_METHODS, true),
    ] {
        for (name, descriptor) in table {
            let candidates = parsed.by_name(name);
            if candidates.is_empty() {
                problems.push(format!(
                    "  {label}: `{name}` — expected {descriptor}\n\
                     \x20     NOT DECLARED on the class at all (renamed, deleted, or moved to a \
                     nested/companion class?)."
                ));
                continue;
            }
            let Some(matched) = candidates.iter().find(|m| m.descriptor == *descriptor) else {
                problems.push(format!(
                    "  {label}: `{name}` — expected {descriptor}\n\
                     \x20     found only: {}",
                    candidates
                        .iter()
                        .map(|m| m.render())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                continue;
            };
            if must_be_native && !matched.is_native() {
                problems.push(format!(
                    "  {label}: `{name}{descriptor}` is declared but NOT native — RegisterNatives \
                     will fail at JNI_OnLoad.\n\
                     \x20     javap says: {}",
                    matched.render()
                ));
            }
        }
    }

    // The other direction: a Kotlin `external fun` that the Rust table does not
    // register is never bound, and the first call throws UnsatisfiedLinkError.
    let registered: BTreeMap<&str, &str> = NATIVE_METHODS.iter().copied().collect();
    for member in parsed.methods.iter().filter(|m| m.is_native()) {
        match registered.get(member.name.as_str()) {
            Some(descriptor) if *descriptor == member.descriptor => {}
            Some(_) => { /* already reported above as a descriptor mismatch */ }
            None => problems.push(format!(
                "  UNREGISTERED NATIVE: `{}{}` is declared `external` in Kotlin but is absent \
                 from NATIVE_METHODS.\n\
                 \x20     RegisterNatives never binds it; the first call throws \
                 UnsatisfiedLinkError.",
                member.name, member.descriptor
            )),
        }
    }

    problems
}

// ---------------------------------------------------------------------------
// Skipping, loudly
// ---------------------------------------------------------------------------

fn skip(reason: &str, remedy: &str) {
    let message = format!(
        "\n\
         ---------------------------------------------------------------\n\
         SKIPPED: the JNI signature check did NOT run.\n\
         ---------------------------------------------------------------\n\
         reason : {reason}\n\
         to run  : {remedy}\n\
         \n\
         Until then nothing verifies that the Kotlin bridge matches the Rust\n\
         contract, and a mismatch surfaces only as NoSuchMethodError on a device.\n\
         Set {REQUIRED_ENV}=1 to make this a hard failure (CI does).\n\
         ---------------------------------------------------------------\n"
    );
    if env::var_os(REQUIRED_ENV).is_some_and(|v| !v.is_empty() && v != "0") {
        panic!("{message}");
    }
    // libtest swallows the output of a passing test, so this is only visible with
    // `-- --show-output` (or `--nocapture`). That is why CI sets REQUIRED_ENV.
    println!("{message}");
}

// ---------------------------------------------------------------------------
// Parser tests against real `javap` output
// ---------------------------------------------------------------------------
//
// These run everywhere, with or without a JDK, so the parser itself is never the
// thing that silently stops working. Every fixture below is verbatim output from
// `javap -s -p` on a genuinely compiled class — not hand-written.

/// `javap -s -p` on a class compiled from the contract, including decoys that Kotlin
/// really does emit: `$`-suffixed synthetics, a same-name overload, a generic method,
/// fields, a constructor and a `static {}` block.
const FIXTURE_MATCHING: &str = r#"
Compiled from "MeshBleBridge.java"
public final class com.chatter.client.MeshBleBridge {
  private static final com.chatter.client.MeshBleBridge$Companion Companion;
    descriptor: Lcom/chatter/client/MeshBleBridge$Companion;
  private static final java.lang.String TAG;
    descriptor: Ljava/lang/String;
  private final byte[] scratch;
    descriptor: [B
  public com.chatter.client.MeshBleBridge();
    descriptor: ()V

  public void attach(android.content.Context);
    descriptor: (Landroid/content/Context;)V

  public boolean start();
    descriptor: ()Z

  public void stop();
    descriptor: ()V

  public boolean setScanning(boolean);
    descriptor: (Z)Z

  public void setLocalPeer(java.lang.String);
    descriptor: (Ljava/lang/String;)V

  public java.lang.String connect(java.lang.String);
    descriptor: (Ljava/lang/String;)Ljava/lang/String;

  public void disconnect(java.lang.String);
    descriptor: (Ljava/lang/String;)V

  public boolean send(java.lang.String, byte[]);
    descriptor: (Ljava/lang/String;[B)Z

  public boolean send(java.lang.String);
    descriptor: (Ljava/lang/String;)Z

  public int maxWrite(java.lang.String);
    descriptor: (Ljava/lang/String;)I

  private static void send$lambda$3(com.chatter.client.MeshBleBridge, java.lang.String);
    descriptor: (Lcom/chatter/client/MeshBleBridge;Ljava/lang/String;)V

  public static boolean setScanning$default(com.chatter.client.MeshBleBridge, boolean, int, java.lang.Object);
    descriptor: (Lcom/chatter/client/MeshBleBridge;ZILjava/lang/Object;)Z

  public static final int access$getFoo$p(com.chatter.client.MeshBleBridge);
    descriptor: (Lcom/chatter/client/MeshBleBridge;)I

  public <T> T identity(T);
    descriptor: (Ljava/lang/Object;)Ljava/lang/Object;

  public java.util.List<java.lang.String> peers();
    descriptor: ()Ljava/util/List;

  private native void nativeOnDiscovered(java.lang.String, int, java.lang.String);
    descriptor: (Ljava/lang/String;ILjava/lang/String;)V

  private native void nativeOnConnected(java.lang.String, boolean, int);
    descriptor: (Ljava/lang/String;ZI)V

  private native void nativeOnDisconnected(java.lang.String);
    descriptor: (Ljava/lang/String;)V

  private native void nativeOnData(java.lang.String, byte[]);
    descriptor: (Ljava/lang/String;[B)V

  private native void nativeOnAdapterPowerChanged(boolean);
    descriptor: (Z)V

  static {};
    descriptor: ()V
}
"#;

/// The same class after four kinds of drift: a changed return type, two deleted
/// methods, a `Boolean`→`Int` parameter, and an extra unregistered native.
const FIXTURE_DRIFTED: &str = r#"
Compiled from "MeshBleBridge.java"
public final class com.chatter.client.MeshBleBridge {
  public com.chatter.client.MeshBleBridge();
    descriptor: ()V

  public boolean start();
    descriptor: ()Z

  public void stop();
    descriptor: ()V

  public void setScanning(boolean);
    descriptor: (Z)V

  public void disconnect(java.lang.String);
    descriptor: (Ljava/lang/String;)V

  public boolean send(java.lang.String, byte[]);
    descriptor: (Ljava/lang/String;[B)Z

  private native void nativeOnDiscovered(java.lang.String, int);
    descriptor: (Ljava/lang/String;I)V

  private native void nativeOnConnected(java.lang.String, boolean, int);
    descriptor: (Ljava/lang/String;ZI)V

  private native void nativeOnDisconnected(java.lang.String);
    descriptor: (Ljava/lang/String;)V

  private native void nativeOnData(java.lang.String, byte[]);
    descriptor: (Ljava/lang/String;[B)V

  private native void nativeOnAdapterPowerChanged(int);
    descriptor: (I)V

  private native void nativeOnRssi(java.lang.String, int);
    descriptor: (Ljava/lang/String;I)V
}
"#;

/// Excerpt of `javap -s -p` on this app's own `MainActivity.class`, as produced by
/// kotlinc (not javac) — real Kotlin output with `final` on members, lambdas and a
/// generic field type. Non-contiguous excerpt of a longer listing.
const FIXTURE_KOTLINC: &str = r#"
Compiled from "MainActivity.kt"
public final class com.chatter.client.MainActivity extends com.google.androidgamesdk.GameActivity {
  private static final com.chatter.client.MainActivity$Companion Companion;
    descriptor: Lcom/chatter/client/MainActivity$Companion;
  private final androidx.activity.result.ActivityResultLauncher<java.lang.String[]> openDocuments;
    descriptor: Landroidx/activity/result/ActivityResultLauncher;
  public static final java.lang.String TAG;
    descriptor: Ljava/lang/String;
  public com.chatter.client.MainActivity();
    descriptor: ()V

  protected void onCreate(android.os.Bundle);
    descriptor: (Landroid/os/Bundle;)V

  private final native void nativeInitWebrtc();
    descriptor: ()V

  private final native void nativeOnCameraFrame(byte[], int, int, int, long);
    descriptor: ([BIIIJ)V

  public final boolean hasPermission(java.lang.String);
    descriptor: (Ljava/lang/String;)Z

  private final byte[] yuv420ToI420(androidx.camera.core.ImageProxy);
    descriptor: (Landroidx/camera/core/ImageProxy;)[B

  private static final void startCamera$lambda$13$lambda$12(com.google.common.util.concurrent.ListenableFuture, com.chatter.client.MainActivity, boolean);
    descriptor: (Lcom/google/common/util/concurrent/ListenableFuture;Lcom/chatter/client/MainActivity;Z)V

  static {};
    descriptor: ()V
}
"#;

#[test]
fn parses_the_class_name() {
    assert_eq!(
        parse_javap(FIXTURE_MATCHING).class_name.as_deref(),
        Some("com.chatter.client.MeshBleBridge")
    );
    assert_eq!(
        parse_javap(FIXTURE_KOTLINC).class_name.as_deref(),
        Some("com.chatter.client.MainActivity")
    );
}

#[test]
fn fields_constructors_and_static_blocks_are_not_methods() {
    let parsed = parse_javap(FIXTURE_MATCHING);
    // Fields have non-`(` descriptors and must not be collected at all.
    for field in ["Companion", "TAG", "scratch"] {
        assert!(
            parsed.by_name(field).is_empty(),
            "field `{field}` leaked into the method table"
        );
    }
    // The constructor and `static {}` both have descriptor ()V; only the constructor
    // keeps a name, and it is never one of ours.
    assert_eq!(parsed.by_name("<init>").len(), 1);
    assert!(parsed.by_name("MeshBleBridge").is_empty());
}

#[test]
fn synthetic_and_overloaded_members_do_not_confuse_lookup() {
    let parsed = parse_javap(FIXTURE_MATCHING);
    // `send$lambda$3` must not answer for `send`; the two real `send` overloads must
    // both be visible so the exact descriptor can be picked out.
    let sends: Vec<&str> = parsed
        .by_name("send")
        .iter()
        .map(|m| m.descriptor.as_str())
        .collect();
    assert_eq!(sends, vec!["(Ljava/lang/String;[B)Z", "(Ljava/lang/String;)Z"]);
    assert_eq!(parsed.by_name("send$lambda$3").len(), 1);
    assert_eq!(parsed.by_name("setScanning").len(), 1, "`$default` is a distinct name");
    // Generics are erased in the descriptor and must be read from there, not the
    // pretty-printed signature.
    assert_eq!(
        parsed.by_name("identity")[0].descriptor,
        "(Ljava/lang/Object;)Ljava/lang/Object;"
    );
}

#[test]
fn the_native_modifier_survives_kotlinc_formatting() {
    // kotlinc writes `private final native void …`; javac writes `private native void …`.
    let kotlin = parse_javap(FIXTURE_KOTLINC);
    assert!(kotlin.by_name("nativeInitWebrtc")[0].is_native());
    assert!(kotlin.by_name("nativeOnCameraFrame")[0].is_native());
    assert!(!kotlin.by_name("hasPermission")[0].is_native());
    assert_eq!(kotlin.by_name("nativeOnCameraFrame")[0].descriptor, "([BIIIJ)V");
    assert_eq!(kotlin.by_name("yuv420ToI420")[0].descriptor, "(Landroidx/camera/core/ImageProxy;)[B");

    let java = parse_javap(FIXTURE_MATCHING);
    assert!(java.by_name("nativeOnData")[0].is_native());
    assert!(!java.by_name("send")[0].is_native());
}

#[test]
fn a_matching_class_produces_no_problems() {
    let problems = check(&parse_javap(FIXTURE_MATCHING));
    assert!(problems.is_empty(), "expected a clean bill of health, got:\n{}", problems.join("\n"));
}

#[test]
fn every_kind_of_drift_is_reported() {
    let problems = check(&parse_javap(FIXTURE_DRIFTED)).join("\n");

    // Wrong return type: Kotlin `fun setScanning(on: Boolean)` vs contract `(Z)Z`.
    assert!(problems.contains("`setScanning` — expected (Z)Z"), "{problems}");
    assert!(problems.contains("(Z)V"), "{problems}");
    // Deleted / renamed methods.
    assert!(problems.contains("`connect` — expected"), "{problems}");
    assert!(problems.contains("`maxWrite` — expected"), "{problems}");
    assert!(problems.contains("NOT DECLARED"), "{problems}");
    // Int where the contract says Boolean — the classic silent mismatch.
    assert!(problems.contains("`nativeOnAdapterPowerChanged` — expected (Z)V"), "{problems}");
    // An `external fun` nobody registered.
    assert!(problems.contains("UNREGISTERED NATIVE: `nativeOnRssi(Ljava/lang/String;I)V`"), "{problems}");
    // Methods that are fine must not be reported.
    assert!(!problems.contains("`start`"), "{problems}");
    assert!(!problems.contains("`stop`"), "{problems}");
    assert!(!problems.contains("`nativeOnData`"), "{problems}");
}

#[test]
fn a_renamed_or_moved_class_is_reported() {
    let moved = FIXTURE_MATCHING.replace("com.chatter.client.MeshBleBridge", "com.chatter.mesh.BleBridge");
    let problems = check(&parse_javap(&moved)).join("\n");
    assert!(problems.contains("class name:"), "{problems}");
    assert!(problems.contains("com.chatter.mesh.BleBridge"), "{problems}");
}

#[test]
fn the_class_path_is_derived_from_the_rust_constant() {
    // A package move in lib.rs must move the search target with it, or the test would
    // keep passing against a class that no longer exists.
    assert!(class_relative_path().ends_with("com/chatter/client/MeshBleBridge.class"));
}
