use super::*;

/// The server's stable token for [`AuthError::LoginGuessLimit`]
/// (auth.proto `LoginStartResponse`), carried in a FAILED_PRECONDITION.
const LOGIN_GUESS_LIMIT_TOKEN: &str = "login_guess_limit:";

/// The server's stable token for a trusted-device token it no longer honours
/// (password changed, recovered, expired), carried in a FAILED_PRECONDITION.
const DEVICE_TOKEN_INVALID_TOKEN: &str = "device_token_invalid:";

/// How often a solved login challenge is resent when the server says its
/// verification budget is momentarily spent (RESOURCE_EXHAUSTED "pow_busy:"),
/// and how long to wait between. The challenge is not consumed by that
/// refusal, so resending the same solution is all it takes.
const POW_BUSY_RETRIES: usize = 3;
const POW_BUSY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(1500);

fn login_start_error(e: NetworkError) -> AuthError {
    match &e {
        NetworkError::InvalidRequest(m) if m.contains(LOGIN_GUESS_LIMIT_TOKEN) => {
            AuthError::LoginGuessLimit
        }
        _ => AuthError::Network(e),
    }
}

/// Prove the current password to the server for the authenticated account
/// (OPAQUE KE1 -> KE2 -> KE3 against its own record). The returned proof is
/// single-use and is spent by the RPC it rides in (RegenerateRecoveryCodes,
/// DeleteAccount, VerifyPassword). A wrong password fails HERE, client-side,
/// with [`AuthError::InvalidCredentials`]: KE2 does not open, so there is no
/// KE3 to send.
pub(crate) async fn prove_password(
    auth_client: &mut AuthClient,
    password: &str,
) -> Result<crate::proto::chat_v1::PasswordProof, AuthError> {
    let start = opaque::login_start(password)?;
    let opened = auth_client.password_proof_start(&start.ke1).await?;
    let done = opaque::login_finish_async(start, password, opened.ke2).await?;
    Ok(crate::proto::chat_v1::PasswordProof {
        proof_id: opened.proof_id,
        ke3: done.ke3,
    })
}

/// How long a login proof of work may run. The server keeps the challenge
/// 3 minutes; the cap difficulty (9 bits = 512 expected Argon2id
/// evaluations) is ESTIMATED at mean ~5 s, p90 ~12 s on a Galaxy S9 (server
/// `login_pow.go` `LoginPoWCapBits`; not measured on the device).
const LOGIN_POW_DEADLINE: std::time::Duration = std::time::Duration::from_secs(150);

/// Most proofs of work one login pays. The server can ask twice: under onion
/// load it prices the first attempt at the transport's effort without looking
/// at the account, and the account's own price (or a price that rose while
/// solving) comes back as a second challenge. A third means something is
/// wrong; give up rather than spin.
const LOGIN_POW_ROUNDS: usize = 3;

/// OPAQUE LoginStart, paying the server's price when it names one (server
/// OPEN-ITEMS §133, §134). A `pow_challenge` instead of a KE2 is solved off
/// the async runtime ([`pow::solve_async`]) and the call repeated with the
/// token and solution, up to [`LOGIN_POW_ROUNDS`] times. `on_verifying` fires
/// before the first solve so the caller can show that the login is working,
/// not stuck.
///
/// [`pow::solve_async`]: crate::crypto::pow::solve_async
///
/// `login_grant` is a recovery's grant (`UseRecoveryCodeFinish`): it skips the
/// price and the account's guess ceiling. A refused grant (expired, already
/// used) falls back to an ordinary attempt. The ceiling itself surfaces as
/// [`AuthError::LoginGuessLimit`].
///
/// `device_token` is this device's trusted-device token for `identifier`
/// (from its last login or registration): with it the server meters this
/// account's trusted reserve instead of the account's untrusted price and
/// ceiling, so a stranger burning the ceiling cannot keep it out. A token the
/// server no longer honours (password changed or recovered, expired) is set
/// to `None` here — the caller deletes its stored copy — and the login goes
/// on without it.
pub async fn login_start_paying_pow(
    auth_client: &mut AuthClient,
    identifier: &str,
    ke1: &[u8],
    login_grant: Option<&[u8]>,
    device_token: &mut Option<Vec<u8>>,
    on_verifying: &(dyn Fn() + Send + Sync),
) -> Result<crate::proto::chat_v1::LoginStartResponse, AuthError> {
    use crate::crypto::pow;
    if let Some(grant) = login_grant {
        match auth_client
            .login_start(identifier, ke1, None, grant, &[])
            .await
        {
            Ok(r) => return Ok(r),
            Err(NetworkError::PermissionDenied) => {
                tracing::warn!("recovery login grant refused; logging in without it");
            }
            Err(e) => return Err(login_start_error(e)),
        }
    }
    let mut response = login_start_once(auth_client, identifier, ke1, None, device_token).await?;
    let started = std::time::Instant::now();
    for round in 0..LOGIN_POW_ROUNDS {
        let Some(challenge) = response.pow_challenge.take() else {
            return Ok(response);
        };
        if round == 0 {
            on_verifying();
        }
        let solve_started = std::time::Instant::now();
        let solution = pow::solve_async(
            pow::Purpose::Login,
            challenge.token.clone(),
            challenge.difficulty,
            challenge.gate_bits,
            started + LOGIN_POW_DEADLINE,
        )
        .await?;
        tracing::info!(
            pow_bits = challenge.difficulty,
            gate_bits = challenge.gate_bits,
            solve_ms = solve_started.elapsed().as_millis() as u64,
            "login proof of work solved"
        );
        let mut busy = 0;
        response = loop {
            match login_start_once(
                auth_client,
                identifier,
                ke1,
                Some((&challenge.token, solution)),
                device_token,
            )
            .await
            {
                Err(AuthError::Network(NetworkError::RateLimited)) if busy < POW_BUSY_RETRIES => {
                    busy += 1;
                    tracing::info!(
                        attempt = busy,
                        "server busy verifying proof of work; resending"
                    );
                    tokio::time::sleep(POW_BUSY_BACKOFF).await;
                }
                other => break other?,
            }
        };
    }
    if response.pow_challenge.is_some() {
        return Err(AuthError::LoginFailed(
            "the server kept asking for proof of work; try again".to_string(),
        ));
    }
    Ok(response)
}

/// One LoginStart presenting `device_token` if there is one. A token the
/// server refuses as `device_token_invalid:` is dropped (set to `None`) and
/// login starts again without a solution: the server may already have spent
/// the paid challenge before checking that token. Reusing it would turn a
/// recoverable stale token into a rejected replay under load.
async fn login_start_once(
    auth_client: &mut AuthClient,
    identifier: &str,
    ke1: &[u8],
    pow: Option<(&[u8], u64)>,
    device_token: &mut Option<Vec<u8>>,
) -> Result<crate::proto::chat_v1::LoginStartResponse, AuthError> {
    if let Some(tok) = device_token.as_deref() {
        match auth_client
            .login_start(identifier, ke1, pow, &[], tok)
            .await
        {
            Err(NetworkError::InvalidRequest(m)) if m.contains(DEVICE_TOKEN_INVALID_TOKEN) => {
                tracing::warn!("trusted device token no longer valid; logging in without it");
                *device_token = None;
                return auth_client
                    .login_start(identifier, ke1, None, &[], &[])
                    .await
                    .map_err(login_start_error);
            }
            other => return other.map_err(login_start_error),
        }
    }
    auth_client
        .login_start(identifier, ke1, pow, &[], &[])
        .await
        .map_err(login_start_error)
}
