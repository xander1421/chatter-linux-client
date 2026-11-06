# MessageList navigation regression

From the repository root:

```sh
CHATTER_NULL_AUDIO=1 cargo test --manifest-path linux-client/tests/message-navigation/Cargo.toml -j 3
```

This standalone workspace compiles the actual `ui/chat/message_list.slint` and
its dependencies against the local patched compiler and core. `build.rs` makes
a temporary copy with test accessors; it does not duplicate navigation logic.
The software backend renders real virtualized delegates between timer ticks.
No application/media/backend build or device is needed.

The single process test covers three-sample measured landing, input cancellation,
inactive/loading cancellation, target replacement, same-ID model replacement,
invalid targets, unread opening, a search command queued during loading,
conditional tail follow, a replaced command's independent deadline, and expiry
without rendering. Timer tests use wall time (roughly seventeen seconds overall).
Unread opening may complete through its deadline if measured geometry keeps
rebasing; its test asserts the retained measured anchor and bounded shutdown.
Ordinary landing separately requires three stable samples. This does not
replace full application or Android gesture/device coverage.
