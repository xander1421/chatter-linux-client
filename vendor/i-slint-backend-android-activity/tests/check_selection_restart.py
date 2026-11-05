#!/usr/bin/env python3
"""Host regression for the real connection override, with an Android API seam.

Run with KOTLINC=/path/to/kotlinc python3 tests/check_selection_restart.py.
This does not emulate an OEM keyboard or replace device testing.
"""
import os
from pathlib import Path
import subprocess
import tempfile


client = Path(__file__).resolve().parents[3]
source = (client / "android/app/src/main/java/com/chatter/client/MainActivity.kt").read_text()
start = source.index("        override fun onCreateInputConnection(")
end = source.index("\n        override fun onKeyPreIme", start)
override = source[start:end].replace(
    "com.google.androidgamesdk.gametextinput.InputConnection", "Connection"
)

# Model the upstream surface returning cached EditorInfo after setState changed
# the Editable. Execute the production override, then let a keyboard trust the
# returned initial selection when deleting/replacing text (the S9 failure).
harness = r'''
class EditorInfo(var initialSelStart: Int = -1, var initialSelEnd: Int = -1, var imeOptions: Int = 0) {
    companion object { const val IME_FLAG_NO_PERSONALIZED_LEARNING: Int = 0x1000000 }
}
class Editable(val text: String, val start: Int, val end: Int)
object Selection {
    fun getSelectionStart(e: Editable) = e.start
    fun getSelectionEnd(e: Editable) = e.end
}
class Connection(val editable: Editable)
open class Surface(val connection: Connection?, val cached: EditorInfo) {
    open fun onCreateInputConnection(outAttrs: EditorInfo): Connection? {
        outAttrs.initialSelStart = cached.initialSelStart
        outAttrs.initialSelEnd = cached.initialSelEnd
        return connection
    }
}
class Fixed(connection: Connection?, cached: EditorInfo): Surface(connection, cached) {
    var imeHidePending: Boolean = true
OVERRIDE
}
fun main() {
    for (text in listOf("typingx rasp world", "alpha bravo charlierld", "română 😀 word")) {
        for ((start, end) in listOf(0 to text.length, text.length to 0, 2 to 2)) {
            val connection = Connection(Editable(text, start, end))
            val stale = EditorInfo(0, 5)
            val attrs = EditorInfo()
            Surface(connection, stale).onCreateInputConnection(attrs)
            check(attrs.initialSelEnd != end) // The upstream restart is stale.
            val fixed = Fixed(connection, stale)
            check(fixed.onCreateInputConnection(attrs) === connection)
            check(!fixed.imeHidePending)
            check(attrs.imeOptions and EditorInfo.IME_FLAG_NO_PERSONALIZED_LEARNING != 0)
            check(attrs.initialSelStart == start && attrs.initialSelEnd == end)
            for (replacement in listOf("", "x")) {
                val result = text.replaceRange(minOf(attrs.initialSelStart, attrs.initialSelEnd),
                    maxOf(attrs.initialSelStart, attrs.initialSelEnd), replacement)
                if (start != end) check(result == replacement)
            }
        }
    }
    check(Fixed(null, EditorInfo()).onCreateInputConnection(EditorInfo()) == null)
    println("PASS: selection restart, delete/replace, reversed selection, UTF-16, closed IME")
}
'''.replace("OVERRIDE", override)

with tempfile.TemporaryDirectory(prefix="chatter-selection-") as temporary:
    root = Path(temporary)
    (root / "SelectionRestart.kt").write_text(harness)
    subprocess.run([os.environ.get("KOTLINC", "kotlinc"), str(root / "SelectionRestart.kt"),
                    "-include-runtime", "-d", str(root / "check.jar")], check=True)
    subprocess.run(["java", "-jar", str(root / "check.jar")], check=True)
