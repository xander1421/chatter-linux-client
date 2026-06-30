**Title:** Two Rust crates each bundle their own libcrypto (OpenSSL + BoringSSL) and fight over the same symbols — what's the right way to deal with this?

---

Spent way too long on this one so I'm hoping someone's hit it before.

I've got a desktop app in Rust. Two of my dependencies each drag in their own copy of a C crypto library:

- `rusqlite` with `bundled-sqlcipher` — that's SQLCipher for the encrypted local DB, and it builds against OpenSSL.
- `livekit` (pulls in `webrtc-sys`) for voice/video — and webrtc statically bundles BoringSSL.

BoringSSL being a fork of OpenSSL is the whole problem: both libraries export the exact same symbol names (`EVP_*`, `HMAC`, `PKCS5_PBKDF2_HMAC`, `AES_*`, the usual) but the implementations behind those names are not compatible — structs are laid out differently, etc.

So linking both into one binary throws duplicate symbol errors. `LNK1169` on MSVC, `multiple definition` from mold on Linux.

The "fix" that's currently in my repo (and I hate it):

```
# .cargo/config.toml
[target.x86_64-pc-windows-msvc]
rustflags = ["-C", "link-arg=/FORCE:MULTIPLE"]

[target.x86_64-unknown-linux-gnu]
rustflags = ["-C", "link-arg=-Wl,--allow-multiple-definition"]
```

It links. But all that flag does is tell the linker "see a duplicate? keep the first one, throw the rest away." So now the whole binary uses ONE implementation for those symbols, and which one you get depends on link order. In my case SQLCipher — which was compiled against OpenSSL's headers — ended up calling into BoringSSL's implementation. Different struct layout, so the first time it actually does crypto (the PBKDF2 in `PRAGMA key` when it opens the encrypted DB) it scribbles over memory and the process dies. Native access violation, no Rust panic, took me ages to even realize it was a linking thing and not a logic bug.

What I've actually got working: I stopped shipping two cryptos. I pointed SQLCipher's `OPENSSL_LIB_DIR`/`OPENSSL_INCLUDE_DIR` at the BoringSSL that webrtc already bundles, so the binary has exactly one libcrypto and there's nothing to collide. SQLCipher 4.5.x is BoringSSL-compatible so it builds and runs. But it feels held together with tape — I'm leaning on webrtc-sys's prebuilt BoringSSL headers being there and not changing, and on Windows I also had to throw `-DNOCRYPT` at the SQLite build because `windows.h` drags in `wincrypt.h` whose macros clash with BoringSSL's typedefs (OpenSSL's headers `#undef` those, BoringSSL doesn't).

The other option is just dropping LiveKit when I don't need calls, which kills webrtc and therefore BoringSSL and the whole conflict — but I want calls and an encrypted DB in the same build, so that's a cop-out.

So what I'm actually asking:

Is there a clean way to make two C libs that export the same symbol names coexist, where each one calls its own crypto? I keep thinking there should be a way to prefix or localize the symbols of one of them (objcopy `--redefine-syms`, `--localize-symbols`, a version script, something) so they stop stepping on each other — but I'm working with prebuilt static libs and I really don't want to rebuild webrtc from source. Has anyone pulled that off with prebuilt `.a`/`.lib` files?

Or is "just unify everything on BoringSSL" genuinely the accepted answer and I should stop fighting it?

And more generally — is there a convention for `-sys` crates along the lines of "I vendor my own crypto, please don't leak it into the global symbol namespace"? Feels like this should be a solved problem and I'm just missing the obvious move.

Stack is Rust stable, targeting both MSVC and Linux, `rusqlite` (bundled-sqlcipher) and `livekit`/`webrtc-sys`. Can paste exact versions and link flags if it helps.
