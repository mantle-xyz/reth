//! Websocket URLs as they may appear in logs and metric labels.
//!
//! A flashblocks endpoint behind `mantle-ws-proxy` carries its API key in the
//! path (`/ws/<key>`), and a URL may also carry credentials in its userinfo or
//! query. None of that may reach a log line or a metric label.

/// The URL with everything that can carry a secret replaced: userinfo and query
/// dropped, the segment after `/ws/` shown as `***`, and any other path shown
/// as `/***`. Scheme, host and port are kept.
///
/// String-based so it serves every URL type in use (`url::Url`, `http::Uri`).
/// Input without a scheme is replaced whole.
pub fn redact_ws_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return "***".to_string();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = host_port(&rest[..authority_end]);
    let path = rest[authority_end..].split(['?', '#']).next().unwrap_or("");

    let path = match path {
        "" | "/" => path,
        _ if path.starts_with("/ws/") => "/ws/***",
        _ => "/***",
    };
    format!("{scheme}://{host}{path}")
}

/// Just `host:port` (or `host`), for a metric label.
pub fn ws_url_endpoint(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    host_port(&rest[..authority_end]).to_string()
}

/// The authority without its userinfo.
fn host_port(authority: &str) -> &str {
    authority.rsplit_once('@').map_or(authority, |(_, host)| host)
}

#[cfg(test)]
mod tests {
    use super::{redact_ws_url, ws_url_endpoint};

    #[test]
    fn a_key_in_the_path_is_hidden() {
        assert_eq!(redact_ws_url("ws://proxy:8545/ws/secret"), "ws://proxy:8545/ws/***");
        assert_eq!(redact_ws_url("wss://proxy/ws/secret/filter"), "wss://proxy/ws/***");
        assert_eq!(redact_ws_url("ws://proxy:8545/v1/secret"), "ws://proxy:8545/***");
    }

    #[test]
    fn userinfo_and_query_are_dropped() {
        assert_eq!(
            redact_ws_url("ws://user:pass@proxy:8545/?token=secret&block_number=7"),
            "ws://proxy:8545/",
        );
        assert_eq!(redact_ws_url("ws://proxy:8545?token=secret"), "ws://proxy:8545");
    }

    #[test]
    fn a_url_with_nothing_secret_is_unchanged() {
        assert_eq!(redact_ws_url("ws://127.0.0.1:1111"), "ws://127.0.0.1:1111");
        assert_eq!(redact_ws_url("ws://[::1]:1111/"), "ws://[::1]:1111/");
    }

    #[test]
    fn input_without_a_scheme_is_hidden_whole() {
        assert_eq!(redact_ws_url("proxy:8545/ws/secret"), "***");
    }

    #[test]
    fn the_endpoint_is_host_and_port_only() {
        assert_eq!(ws_url_endpoint("ws://user:pass@proxy:8545/ws/secret?x=1"), "proxy:8545");
        assert_eq!(ws_url_endpoint("ws://proxy/ws/secret"), "proxy");
    }
}
