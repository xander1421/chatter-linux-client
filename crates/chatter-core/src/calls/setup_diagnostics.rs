//! Release-safe setup phases. Only constant metadata targets are emitted; no
//! identifiers, errors, keys, URLs, candidate addresses or event values.
use std::future::Future;

macro_rules! phases {
    ($($phase:ident => ($begin:literal, $ok:literal, $failed:literal, $cancelled:literal)),+ $(,)?) => {
        #[derive(Clone, Copy)]
        pub(crate) enum Phase { $($phase),+ }
        impl Phase {
            fn record(self, outcome: Outcome) {
                match (self, outcome) {
                    $((Self::$phase, Outcome::Begin) => tracing::info!(target: $begin, "Call setup diagnostic"),
                    (Self::$phase, Outcome::Succeeded) => tracing::info!(target: $ok, "Call setup diagnostic"),
                    (Self::$phase, Outcome::Failed) => tracing::warn!(target: $failed, "Call setup diagnostic"),
                    (Self::$phase, Outcome::Cancelled) => tracing::info!(target: $cancelled, "Call setup diagnostic")),+
                }
            }
        }
    };
}
phases! {
    OrchestratorAccept => ("call_setup::orchestrator_accept_begin", "call_setup::orchestrator_accept_succeeded", "call_setup::orchestrator_accept_failed", "call_setup::orchestrator_accept_cancelled"),
    DmCrypto => ("call_setup::dm_crypto_begin", "call_setup::dm_crypto_succeeded", "call_setup::dm_crypto_failed", "call_setup::dm_crypto_cancelled"),
    DmOffer => ("call_setup::dm_offer_begin", "call_setup::dm_offer_succeeded", "call_setup::dm_offer_failed", "call_setup::dm_offer_cancelled"),
    DmAnswer => ("call_setup::dm_answer_begin", "call_setup::dm_answer_succeeded", "call_setup::dm_answer_failed", "call_setup::dm_answer_cancelled"),
    CreateRpc => ("call_setup::create_rpc_begin", "call_setup::create_rpc_succeeded", "call_setup::create_rpc_failed", "call_setup::create_rpc_cancelled"),
    JoinRpc => ("call_setup::join_rpc_begin", "call_setup::join_rpc_succeeded", "call_setup::join_rpc_failed", "call_setup::join_rpc_cancelled"),
}
enum Outcome {
    Begin,
    Succeeded,
    Failed,
    Cancelled,
}

struct Pending(Option<Phase>);
impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(phase) = self.0 {
            phase.record(Outcome::Cancelled);
        }
    }
}

/// Observe the actual awaited operation, including cancellation. Never formats
/// or otherwise inspects its result payload; caller retains the exact result.
pub(crate) async fn observe<T, E>(
    phase: Phase,
    future: impl Future<Output = Result<T, E>>,
) -> Result<T, E> {
    phase.record(Outcome::Begin);
    let mut pending = Pending(Some(phase));
    let result = future.await;
    phase.record(if result.is_ok() {
        Outcome::Succeeded
    } else {
        Outcome::Failed
    });
    pending.0 = None;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::{Layer, prelude::*};

    struct Targets(Arc<Mutex<Vec<&'static str>>>);
    impl<S: tracing::Subscriber> Layer<S> for Targets {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            assert!(event.metadata().fields().field("error").is_none());
            self.0.lock().unwrap().push(event.metadata().target());
        }
    }
    struct PrivateError;
    impl std::fmt::Debug for PrivateError {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("private error formatted")
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn preserves_results_and_distinguishes_pending_failure_and_cancellation() {
        let targets = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Targets(targets.clone()));
        let _guard = tracing::subscriber::set_default(subscriber);
        let mut pending = Box::pin(observe(
            Phase::DmCrypto,
            std::future::pending::<Result<(), PrivateError>>(),
        ));
        assert!(futures::poll!(&mut pending).is_pending());
        assert_eq!(*targets.lock().unwrap(), ["call_setup::dm_crypto_begin"]);
        drop(pending);
        let value = observe::<_, PrivateError>(Phase::DmAnswer, async { Ok(17) })
            .await
            .ok();
        assert_eq!(value, Some(17));
        assert!(
            observe::<(), _>(Phase::JoinRpc, async { Err(PrivateError) })
                .await
                .is_err()
        );
        assert_eq!(
            *targets.lock().unwrap(),
            [
                "call_setup::dm_crypto_begin",
                "call_setup::dm_crypto_cancelled",
                "call_setup::dm_answer_begin",
                "call_setup::dm_answer_succeeded",
                "call_setup::join_rpc_begin",
                "call_setup::join_rpc_failed",
            ]
        );
    }
}
