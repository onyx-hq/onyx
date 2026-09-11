//! Who is asking: the socket peer, and whether it is this machine.

use std::net::SocketAddr;

use axum::extract;

/// Loopback in the sense that matters: an IPv4-mapped `::ffff:127.0.0.1` is the
/// same machine as `127.0.0.1`, but `Ipv6Addr::is_loopback` alone says no.
pub(super) fn is_loopback_peer(peer: SocketAddr) -> bool {
    peer.ip().to_canonical().is_loopback()
}

/// The peer address **when the listener supplied one**.
///
/// `ConnectInfo<SocketAddr>` is a mandatory extractor — axum 0.8 gives it no
/// optional impl — so a handler taking it directly returns a bare 500 on any
/// listener served without `into_make_service_with_connect_info`, and on any
/// router-level test that drives the service directly. The internal port was
/// exactly that listener. This extractor never rejects, so the handler decides
/// what an unknown peer means instead of the request dying with no explanation.
///
/// Unknown is treated as **not** loopback everywhere it's consulted: the only
/// thing loopback grants is the un-typed roster fallback, so failing closed
/// costs an explicit `OXY_DEV_LOGIN_EMAILS` and nothing else.
pub struct PeerAddr(pub Option<SocketAddr>);

impl PeerAddr {
    pub(super) fn is_loopback(&self) -> bool {
        self.0.is_some_and(is_loopback_peer)
    }
}

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for PeerAddr {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<extract::ConnectInfo<SocketAddr>>()
                .map(|extract::ConnectInfo(peer)| *peer),
        ))
    }
}

/// Whether the bypass is reachable for a caller, with the process-wide statics
/// injected — same shape as `resolve_source`'s `debug_build` parameter, and for
/// the same reason: a test that re-implements the expression cannot fail when
/// the expression changes.
pub(super) fn reachable(enabled: bool, loopback_only: bool, peer: Option<SocketAddr>) -> bool {
    enabled && (!loopback_only || peer.is_some_and(is_loopback_peer))
}
