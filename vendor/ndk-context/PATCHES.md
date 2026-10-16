# Chatter patch to ndk-context 0.1.1

A boot or package-update receiver can start `MessagingService` before
`GameActivity` exists. The service installs its Application global reference and
VM so core Keystore and notification JNI can run without a window. Android
activity glue later calls `initialize_android_context` for the same VM; the
upstream single-initialization assertion aborts in that order.

This fork serializes context access, permits repeated initialization for the
same VM, and retains the first Application reference for the process lifetime.
Activity teardown cannot invalidate the service's JNI context. The added
`is_initialized` query avoids allocating another global reference if the
Activity initialized the process first.
