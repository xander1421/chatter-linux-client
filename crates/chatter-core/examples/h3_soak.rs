//! Production transport soak using one public, unauthenticated RPC.

use std::error::Error;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chatter_core::testing::network::grpc::{GrpcClientPool, transport::GrpcTransport};
use chatter_proto::chat_v1::{
    KtMonitorRequest, key_transparency_service_client::KeyTransparencyServiceClient,
};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep_until, timeout};

const ORIGIN: &str = "https://chat.offai.app";
const DURATION: u64 = 720;
const SPARSE: [u64; 5] = [60, 150, 270, 420, 600];
const RPC_TIMEOUT: Duration = Duration::from_secs(15);

struct Handle {
    name: &'static str,
    channel: GrpcTransport,
    last_success: Mutex<Option<Instant>>,
}

impl Handle {
    fn new(name: &'static str, channel: GrpcTransport, expect_h3: bool) -> Arc<Self> {
        assert_eq!(
            channel.is_http3(),
            expect_h3,
            "{name}: unexpected transport"
        );
        Arc::new(Self {
            name,
            channel,
            last_success: Mutex::new(None),
        })
    }

    async fn fetch(&self, epoch: Instant, scheduled: u64, request: usize) -> bool {
        let started = Instant::now();
        let since = self
            .last_success
            .lock()
            .unwrap()
            .map(|at| format!("{:.3}", started.saturating_duration_since(at).as_secs_f64()))
            .unwrap_or_else(|| "-".into());
        let mut client = KeyTransparencyServiceClient::new(self.channel.clone());
        let error = match timeout(RPC_TIMEOUT, client.monitor(KtMonitorRequest::default())).await {
            Ok(Ok(response)) => {
                let response = response.into_inner();
                if response.tree_head.and_then(|head| head.tree_head)
                    .is_some_and(|head| head.tree_size > 0)
                    && matches!(response.inclusion.as_slice(), [root] if root.len() == 32)
                {
                    *self.last_success.lock().unwrap() = Some(Instant::now());
                    None
                } else {
                    Some("response did not contain a monitored head".into())
                }
            }
            Ok(Err(status)) => Some(error_chain(&status)),
            Err(elapsed) => Some(error_chain(&elapsed)),
        };
        println!(
            "sample\t{scheduled}\t{:.3}\t{}\t{}\t{request}\t{}\t{since}\t{:.3}\t{}",
            started.saturating_duration_since(epoch).as_secs_f64(),
            self.name,
            if self.channel.is_http3() { "h3" } else { "h2" },
            if error.is_some() { "FAIL" } else { "OK" },
            started.elapsed().as_secs_f64(),
            error.as_deref().unwrap_or("-"),
        );
        error.is_none()
    }
}

fn error_chain(error: &dyn Error) -> String {
    let mut chain = format!("{error} [{error:?}]");
    let mut source = error.source();
    while let Some(cause) = source {
        chain.push_str(&format!(" <- {cause} [{cause:?}]"));
        source = cause.source();
    }
    chain.replace('\n', "\\n").replace('\t', "\\t")
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    if std::env::args().nth(1).as_deref() == Some("--validate-only") {
        println!("soak binary built; runtime validation skipped network access");
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter("info,h3_util::client_conn=trace,h3_util::quinn::client=trace")
        .init();

    let pool = GrpcClientPool::new_for_streaming(ORIGIN)?;
    let control = GrpcClientPool::new_for_streaming(ORIGIN)?;
    control.disable_quic_for_testing();
    // The first lookup returns provisional TCP before the spawned QUIC probe
    // runs. Keep that handle even after later lookups return HTTP/3.
    let t = Handle::new("T", pool.channel(), false);
    let c = Handle::new("control", control.channel(), false);
    println!(
        "sample\tscheduled_s\tstarted_s\thandle\tprotocol\trequest\toutcome\tsince_success_s\telapsed_s\terror_chain"
    );
    let setup = Instant::now();
    // Prime TCP sockets so the first sparse request tests an existing socket.
    let (t_ready, c_ready) = tokio::join!(t.fetch(setup, 0, 0), c.fetch(setup, 0, 0));
    if !t_ready || !c_ready {
        return Err("TCP priming or monitor validation failed".into());
    }
    println!("validated public monitor response");
    let q = timeout(Duration::from_secs(5), async {
        loop {
            let channel = pool.channel();
            if channel.is_http3() {
                break Handle::new("Q", channel, true);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| "pool did not promote to HTTP/3")?;

    // Allow the two priming calls to refill the shared per-IP RPC bucket
    // (2 requests/sec, burst 10) before Q's first nine-call burst.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let epoch = Instant::now();
    println!("soak start: {DURATION}s; Q quiet between +300s and +390s");
    let mut running = JoinSet::<bool>::new();
    let mut failures = 0;
    for second in (0..DURATION).step_by(5) {
        sleep_until(epoch + Duration::from_secs(second)).await;
        while let Some(joined) = running.try_join_next() {
            failures += usize::from(!joined?);
        }
        if !(301..390).contains(&second) {
            let count = if second % 30 == 0 { 9 } else { 1 };
            for request in 1..=count {
                let handle = q.clone();
                running.spawn(async move { handle.fetch(epoch, second, request).await });
            }
        }
        if SPARSE.contains(&second) {
            for handle in [t.clone(), c.clone()] {
                running.spawn(async move {
                    // A Q burst plus T uses 10 tokens; give the shared rate
                    // limiter time to refill one before the control sample.
                    if handle.name == "control" && second % 30 == 0 {
                        sleep_until(epoch + Duration::from_millis(second * 1000 + 750)).await;
                    }
                    handle.fetch(epoch, second, 1).await
                });
            }
        }
    }
    sleep_until(epoch + Duration::from_secs(DURATION)).await;
    while let Some(joined) = running.join_next().await {
        failures += usize::from(!joined?);
    }
    println!("soak complete: {failures} failed samples");
    if failures != 0 {
        return Err(format!("transport soak failed: {failures} RPC samples failed").into());
    }
    Ok(())
}
