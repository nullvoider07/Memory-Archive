// /Memory-Archive/ma-core/src/capture/stream.rs

use std::time::Duration;

use anyhow::{anyhow, Context};
use ma_proto::control_center::{
    control_service_client::ControlServiceClient,
    CommandEvent,
    WatchRequest,
};
use tokio::time::timeout;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint};
use tonic::{Request, Streaming};

use crate::config::CcSecurity;

// DisconnectReason
#[derive(Debug, Clone)]
pub enum DisconnectReason {
    AgentDisconnected,
    SilenceTimeout,
    TransportError(String),
}

// WatchStream
pub struct WatchStream {
    stream: Streaming<CommandEvent>,
    silence_timeout: Duration,
    disconnect_reason: Option<DisconnectReason>,
    transport: Transport,
}

/// Transports to attempt, in order, for a given policy.
///
/// Only `Auto` yields a fallback, and the safe transport is always first, so a
/// downgrade can never happen before TLS has been tried.
fn transport_plan(security: CcSecurity) -> &'static [Transport] {
    match security {
        CcSecurity::Auto => &[Transport::Tls, Transport::Plaintext],
        CcSecurity::Strict => &[Transport::Tls],
        CcSecurity::Legacy => &[Transport::Plaintext],
    }
}

/// Force `addr` to carry the scheme matching `transport`.
///
/// The configured address may name either scheme, or none at all, but a given
/// attempt has already decided which wire it is using.
fn normalize_addr(addr: &str, transport: Transport) -> String {
    let bare = addr
        .strip_prefix("https://")
        .or_else(|| addr.strip_prefix("http://"))
        .unwrap_or(addr);

    match transport {
        Transport::Tls => format!("https://{bare}"),
        Transport::Plaintext => format!("http://{bare}"),
    }
}

/// Everything needed to reach Control-Center, resolved from [`crate::config::Config`].
#[derive(Clone)]
pub struct CcEndpoint {
    pub addr: String,
    pub tls_ca: String,
    pub token: String,
    pub security: CcSecurity,
}

// Debug is written by hand so the token cannot reach a log through a derived
// impl. `tracing` prints whatever it is handed, and a future `?cc` at any call
// site would otherwise publish the credential in plaintext.
impl std::fmt::Debug for CcEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CcEndpoint")
            .field("addr", &self.addr)
            .field("tls_ca", &self.tls_ca)
            .field("token", &if self.token.is_empty() { "(unset)" } else { "(redacted)" })
            .field("security", &self.security)
            .finish()
    }
}

/// Which wire the attempt used. Recorded as session provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Tls,
    Plaintext,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Tls => "tls",
            Transport::Plaintext => "plaintext",
        }
    }
}

/// A failed attempt, split by whether retrying on another transport could help.
///
/// The distinction is load-bearing: a rejected token means TLS already succeeded,
/// so retrying in plaintext would weaken the connection without fixing anything.
enum ConnectFailure {
    /// The connection itself failed — handshake, h2, or the server was unreachable.
    Transport(anyhow::Error),
    /// The server answered and refused the credentials.
    Auth(tonic::Status),
}

impl ConnectFailure {
    fn into_report(self, addr: &str, attempted: Transport) -> anyhow::Error {
        match self {
            ConnectFailure::Auth(status) => anyhow!(
                "Control-Center rejected the credentials for WatchCommands ({}). \
                 Version 1.1.0 and later require a token with the `monitor` scope — \
                 set `control_center_token` in the ma-core config. Mint one with: \
                 CC_JWT_SECRET=<cc jwt_secret> control-center token generate \
                 --user ma-core --scopes monitor",
                status.code()
            ),
            ConnectFailure::Transport(e) => {
                let detail = format!("{e:#}");
                let hint = if detail.contains("UnknownIssuer")
                    || detail.contains("CERTIFICATE_VERIFY_FAILED")
                    || detail.contains("InvalidCertificate")
                {
                    "the server certificate was not trusted — point `control_center_tls_ca` \
                     at the CA that signed it (`control-center gen-certs` writes ca.crt)"
                } else if attempted == Transport::Plaintext {
                    "the server refused a plaintext connection — Control-Center 1.1.0 and \
                     later require TLS unless started with CC_ALLOW_INSECURE=true"
                } else {
                    "could not establish a TLS connection — check that Control-Center is \
                     running and reachable at this address"
                };
                anyhow!("Failed to reach Control-Center at {addr}: {hint} ({detail})")
            }
        }
    }
}

// WatchStream implementation
impl WatchStream {
    pub async fn connect(cc: CcEndpoint, silence_timeout: Duration) -> anyhow::Result<Self> {
        const MAX_ATTEMPTS: u32 = 10;
        const BASE_DELAY_MS: u64 = 500;
        const MAX_DELAY_MS: u64 = 30_000;

        let mut attempt = 0;

        loop {
            attempt += 1;
            match Self::negotiate(&cc).await {
                Ok((stream, transport)) => {
                    tracing::info!(
                        attempt,
                        transport = transport.as_str(),
                        "Connected to Control-Center"
                    );
                    tracing::debug!(addr = %cc.addr, "Control-Center address");
                    return Ok(Self {
                        stream,
                        silence_timeout,
                        disconnect_reason: None,
                        transport,
                    });
                }
                Err(e) => {
                    // A refused token is a configuration error, not a transient one.
                    // Retrying ten times cannot fix it and only delays the report.
                    let fatal = matches!(e, ConnectFailure::Auth(_));
                    let report = e.into_report(&cc.addr, Self::first_transport(cc.security));

                    if fatal || attempt >= MAX_ATTEMPTS {
                        return Err(report);
                    }

                    // Exponential backoff: 500ms, 1s, 2s, 4s ... capped at 30s.
                    let delay_ms = (BASE_DELAY_MS * (1 << (attempt - 1))).min(MAX_DELAY_MS);
                    tracing::warn!(
                        addr = %cc.addr,
                        attempt,
                        next_retry_ms = delay_ms,
                        "Control-Center not ready — retrying: {report:#}"
                    );
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
            }
        }
    }

    fn first_transport(security: CcSecurity) -> Transport {
        match security {
            CcSecurity::Legacy => Transport::Plaintext,
            _ => Transport::Tls,
        }
    }

    /// Try each transport the policy permits, safest first.
    ///
    /// TLS is attempted before plaintext regardless of the URL scheme: an address
    /// configured as `http://` against a 1.1.0+ server would otherwise fail with no
    /// attempt at the transport that would have worked. Pinning is still available
    /// through `CcSecurity::Strict` and `CcSecurity::Legacy`.
    async fn negotiate(
        cc: &CcEndpoint,
    ) -> Result<(Streaming<CommandEvent>, Transport), ConnectFailure> {
        let plan = transport_plan(cc.security);

        let mut last: Option<ConnectFailure> = None;

        for (index, transport) in plan.iter().copied().enumerate() {
            match Self::try_connect(cc, transport).await {
                Ok(stream) => {
                    if index > 0 {
                        // Only reachable under Auto, and only after TLS failed at the
                        // transport level. Never let a downgrade pass unannounced.
                        tracing::warn!(
                            addr = %cc.addr,
                            "TLS to Control-Center failed — downgraded to an unencrypted \
                             connection. This is expected against Control-Center 1.0.0, \
                             which has no TLS support. Set control_center_security = \
                             \"strict\" to refuse the downgrade."
                        );
                    }
                    return Ok((stream, transport));
                }
                // The server answered and refused the token. Another transport
                // cannot help, and trying one would be a pointless downgrade.
                Err(ConnectFailure::Auth(status)) => {
                    return Err(ConnectFailure::Auth(status));
                }
                Err(other) => last = Some(other),
            }
        }

        Err(last.unwrap_or_else(|| {
            ConnectFailure::Transport(anyhow!("no transport permitted by the security policy"))
        }))
    }

    /// Single connection attempt on one transport.
    async fn try_connect(
        cc: &CcEndpoint,
        transport: Transport,
    ) -> Result<Streaming<CommandEvent>, ConnectFailure> {
        let uri = normalize_addr(&cc.addr, transport);

        let mut endpoint = Endpoint::from_shared(uri.clone())
            .with_context(|| format!("Invalid Control-Center address: {uri}"))
            .map_err(ConnectFailure::Transport)?;

        if transport == Transport::Tls {
            let mut tls = ClientTlsConfig::new().with_enabled_roots();
            if !cc.tls_ca.is_empty() {
                let pem = std::fs::read(&cc.tls_ca)
                    .with_context(|| format!("Cannot read control_center_tls_ca: {}", cc.tls_ca))
                    .map_err(ConnectFailure::Transport)?;
                tls = tls.ca_certificate(Certificate::from_pem(pem));
            }
            endpoint = endpoint
                .tls_config(tls)
                .context("Failed to configure TLS for the Control-Center connection")
                .map_err(ConnectFailure::Transport)?;
        }

        let channel = endpoint
            .connect()
            .await
            .with_context(|| format!("Failed to connect to Control-Center at {uri}"))
            .map_err(ConnectFailure::Transport)?;

        let mut client = ControlServiceClient::new(channel);

        // Control-Center 1.1.0+ requires the `monitor` scope here; 1.0.0 never reads
        // the header.
        //
        // The credential travels only over an encrypted channel, or over plaintext
        // the operator asked for by name. An automatic downgrade is not consent: an
        // attacker able to disrupt the TLS handshake would otherwise force the
        // fallback and collect a `monitor`-scoped token in the clear — and that
        // token subscribes to WatchCommands, which carries every keystroke the
        // session records. Withholding it costs nothing against a genuine 1.0.0
        // server, which does not check it.
        let credentials_allowed = transport == Transport::Tls || cc.security == CcSecurity::Legacy;

        let mut request = Request::new(WatchRequest {});
        if !cc.token.is_empty() {
            if credentials_allowed {
                let value = format!("Bearer {}", cc.token)
                    .parse()
                    .context("control_center_token is not a valid HTTP header value")
                    .map_err(ConnectFailure::Transport)?;
                request.metadata_mut().insert("authorization", value);
            } else {
                tracing::warn!(
                    addr = %cc.addr,
                    "Withholding the Control-Center token on an unencrypted fallback \
                     connection. A server that requires it will refuse the stream; set \
                     control_center_security = \"legacy\" to send credentials in the clear \
                     deliberately."
                );
            }
        }

        let response = client.watch_commands(request).await.map_err(|status| {
            match status.code() {
                tonic::Code::Unauthenticated | tonic::Code::PermissionDenied => {
                    ConnectFailure::Auth(status)
                }
                _ => ConnectFailure::Transport(anyhow!(
                    "WatchCommands stream refused: {status}"
                )),
            }
        })?;

        Ok(response.into_inner())
    }

    pub fn transport(&self) -> Transport {
        self.transport
    }

    pub async fn next_event(&mut self) -> Option<CommandEvent> {
        loop {
            let result = timeout(self.silence_timeout, self.stream.message()).await;

            match result {
                Err(_elapsed) => {
                    self.disconnect_reason = Some(DisconnectReason::SilenceTimeout);
                    return None;
                }
                Ok(Err(status)) => {
                    self.disconnect_reason =
                        Some(DisconnectReason::TransportError(status.to_string()));
                    return None;
                }
                Ok(Ok(None)) => {
                    self.disconnect_reason = Some(DisconnectReason::AgentDisconnected);
                    return None;
                }
                Ok(Ok(Some(event))) => {
                    if event.is_heartbeat {
                        continue;
                    }
                    return Some(event);
                }
            }
        }
    }

    pub fn disconnect_reason(&self) -> Option<DisconnectReason> {
        self.disconnect_reason.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_never_offers_a_downgrade() {
        assert_eq!(transport_plan(CcSecurity::Strict), &[Transport::Tls]);
    }

    #[test]
    fn legacy_never_attempts_tls() {
        assert_eq!(transport_plan(CcSecurity::Legacy), &[Transport::Plaintext]);
    }

    #[test]
    fn auto_tries_tls_before_plaintext() {
        // Order is the security property: TLS must be attempted first so that a
        // downgrade only ever follows a real TLS failure.
        assert_eq!(
            transport_plan(CcSecurity::Auto),
            &[Transport::Tls, Transport::Plaintext]
        );
    }

    #[test]
    fn scheme_is_rewritten_to_match_the_attempt() {
        // The configured scheme does not decide the transport under Auto — an
        // http:// address must still be reachable over TLS, or a 1.1.0+ server
        // would be unreachable without the user editing config first.
        assert_eq!(
            normalize_addr("http://192.168.1.9:50051", Transport::Tls),
            "https://192.168.1.9:50051"
        );
        assert_eq!(
            normalize_addr("https://192.168.1.9:50051", Transport::Plaintext),
            "http://192.168.1.9:50051"
        );
    }

    #[test]
    fn bare_address_gains_a_scheme() {
        assert_eq!(
            normalize_addr("192.168.1.9:50051", Transport::Tls),
            "https://192.168.1.9:50051"
        );
        assert_eq!(
            normalize_addr("192.168.1.9:50051", Transport::Plaintext),
            "http://192.168.1.9:50051"
        );
    }

    #[test]
    fn rejected_credentials_report_the_token_setting() {
        let failure = ConnectFailure::Auth(tonic::Status::unauthenticated("no token"));
        let report = format!("{:#}", failure.into_report("https://host:50051", Transport::Tls));
        assert!(report.contains("control_center_token"), "got: {report}");
        assert!(report.contains("monitor"), "got: {report}");
    }

    #[test]
    fn plaintext_refusal_points_at_tls() {
        let failure = ConnectFailure::Transport(anyhow!("transport error"));
        let report = format!(
            "{:#}",
            failure.into_report("http://host:50051", Transport::Plaintext)
        );
        assert!(report.contains("require TLS"), "got: {report}");
    }

    #[test]
    fn untrusted_certificate_points_at_the_ca_setting() {
        let failure = ConnectFailure::Transport(anyhow!("invalid peer certificate: UnknownIssuer"));
        let report = format!("{:#}", failure.into_report("https://host:50051", Transport::Tls));
        assert!(report.contains("control_center_tls_ca"), "got: {report}");
    }

    fn endpoint(security: CcSecurity) -> CcEndpoint {
        CcEndpoint {
            addr: "192.168.1.9:50051".to_string(),
            tls_ca: String::new(),
            token: "super-secret-jwt".to_string(),
            security,
        }
    }

    /// Mirrors the rule in `try_connect`: credentials travel over TLS, or over
    /// plaintext only when the operator named it.
    fn credentials_allowed(cc: &CcEndpoint, transport: Transport) -> bool {
        transport == Transport::Tls || cc.security == CcSecurity::Legacy
    }

    #[test]
    fn credentials_never_ride_an_automatic_downgrade() {
        // The attack this closes: disrupt the TLS handshake, collect a
        // monitor-scoped token in the clear, then subscribe to WatchCommands and
        // read every recorded keystroke.
        let cc = endpoint(CcSecurity::Auto);
        assert!(credentials_allowed(&cc, Transport::Tls));
        assert!(
            !credentials_allowed(&cc, Transport::Plaintext),
            "an auto downgrade must not carry the token"
        );
    }

    #[test]
    fn legacy_may_send_credentials_in_the_clear() {
        // Explicit operator choice, so the credential is allowed to travel.
        let cc = endpoint(CcSecurity::Legacy);
        assert!(credentials_allowed(&cc, Transport::Plaintext));
    }

    #[test]
    fn strict_only_ever_sends_over_tls() {
        let cc = endpoint(CcSecurity::Strict);
        assert!(credentials_allowed(&cc, Transport::Tls));
        assert!(!credentials_allowed(&cc, Transport::Plaintext));
    }

    #[test]
    fn debug_never_prints_the_token() {
        let rendered = format!("{:?}", endpoint(CcSecurity::Auto));
        assert!(
            !rendered.contains("super-secret-jwt"),
            "token leaked through Debug: {rendered}"
        );
        assert!(rendered.contains("(redacted)"), "got: {rendered}");

        let empty = CcEndpoint { token: String::new(), ..endpoint(CcSecurity::Auto) };
        assert!(format!("{empty:?}").contains("(unset)"));
    }
}