//! The only way a plugin reaches the network: https only, the hosts the user left on, no private addresses,
//! and every redirect checked again.

use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

const MAX_REDIRECTS: usize = 5;
const MAX_RESPONSE_BYTES: u64 = 32 * 1024 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 5 * 60_000;

#[derive(Debug, Default, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Body {
    #[default]
    None,
    Text {
        text: String,
    },
    Json {
        json: serde_json::Value,
    },
    Bytes,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: Body,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct HttpReply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

/// Whether `host` is covered by one of `allowed`. `*.example.com` covers subdomains only, never `example.com`.
pub fn host_allowed(host: &str, allowed: &[String]) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || host.parse::<IpAddr>().is_ok() || host.starts_with('[') {
        return false;
    }
    allowed.iter().any(|entry| match entry.strip_prefix("*.") {
        Some(suffix) => host.strip_suffix(suffix).is_some_and(|rest| rest.len() > 1 && rest.ends_with('.')),
        None => host == *entry,
    })
}

pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public_v4(v4),
            None => is_public_v6(v6),
        },
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    let cgnat = a == 100 && (64..128).contains(&b);
    let ietf_protocol = a == 192 && b == 0 && c == 0;
    let benchmarking = a == 198 && (18..20).contains(&b);
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_documentation()
        || a == 0
        || a >= 240
        || cgnat
        || ietf_protocol
        || benchmarking)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    let unique_local = first & 0xfe00 == 0xfc00;
    let link_local = first & 0xffc0 == 0xfe80;
    let site_local = first & 0xffc0 == 0xfec0;
    // These embed an IPv4 address that a NAT64 or an old stack would connect to.
    let nat64 = ip.segments()[..6] == [0x64, 0xff9b, 0, 0, 0, 0];
    let v4_compatible = ip.segments()[..6] == [0, 0, 0, 0, 0, 0];
    !(ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() || unique_local || link_local || site_local || nat64 || v4_compatible)
}

// Checked where the connection is made, so a name can't resolve to a public address for a check and a
// private one for the connect.
#[derive(Debug, Default)]
struct PublicResolver(DefaultResolver);

impl Resolver for PublicResolver {
    fn resolve(&self, uri: &Uri, config: &Config, timeout: NextTimeout) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let addresses = self.0.resolve(uri, config, timeout)?;
        if addresses.iter().any(|address| !is_public_ip(address.ip())) {
            return Err(ureq::Error::Other("The host resolves to a private address, which plugins can't reach".into()));
        }
        Ok(addresses)
    }
}

pub fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(0)
        .http_status_as_error(false)
        .proxy(None)
        .user_agent(concat!("Rona/", env!("CARGO_PKG_VERSION"), " plugin"))
        .build();
    ureq::Agent::with_parts(config, DefaultConnector::default(), PublicResolver::default())
}

fn check_url(url: &str, allowed: &[String]) -> Result<Uri, String> {
    let uri: Uri = url.parse().map_err(|_| format!("\"{url}\" is not a valid link"))?;
    if uri.scheme_str() != Some("https") {
        return Err("Plugins can only use https links".into());
    }
    let authority = uri.authority().ok_or("The link has no host")?;
    if authority.as_str().contains('@') {
        return Err("Links with a user name or password aren't allowed".into());
    }
    if !host_allowed(authority.host(), allowed) {
        return Err(format!("The host {} isn't one this plugin may reach", authority.host()));
    }
    Ok(uri)
}

fn join(base: &Uri, location: &str) -> String {
    if location.contains("://") {
        return location.to_owned();
    }
    let origin = format!("{}://{}", base.scheme_str().unwrap_or("https"), base.authority().map_or("", |authority| authority.as_str()));
    if location.starts_with("//") {
        return format!("{}:{location}", base.scheme_str().unwrap_or("https"));
    }
    if location.starts_with('/') {
        return format!("{origin}{location}");
    }
    let path = base.path();
    let folder = &path[..path.rfind('/').map_or(0, |slash| slash + 1)];
    format!("{origin}{folder}{location}")
}

/// `allowed` is the list of hosts the user has on right now.
pub fn execute(agent: &ureq::Agent, allowed: &[String], request: HttpRequest, bytes: Vec<u8>) -> Result<(HttpReply, Vec<u8>), String> {
    let timeout = Duration::from_millis(request.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).clamp(1, MAX_TIMEOUT_MS));
    let mut method = request.method.to_ascii_uppercase();
    let mut body = match request.body {
        Body::None => Vec::new(),
        Body::Text { text } => text.into_bytes(),
        Body::Json { json } => json.to_string().into_bytes(),
        Body::Bytes => bytes,
    };
    let mut url = request.url;

    for _ in 0..=MAX_REDIRECTS {
        let uri = check_url(&url, allowed)?;
        let mut builder = ureq::http::Request::builder().method(method.as_str()).uri(uri.clone());
        for (name, value) in &request.headers {
            if name.eq_ignore_ascii_case("host") {
                continue;
            }
            builder = builder.header(name.as_str(), value.as_str());
        }
        let built = builder.body(body.clone()).map_err(|err| format!("The request is not valid: {err}"))?;
        let built = agent.configure_request(built).timeout_global(Some(timeout)).build();
        let response = agent.run(built).map_err(|err| err.to_string())?;

        let status = response.status().as_u16();
        let location = response.headers().get("location").and_then(|value| value.to_str().ok()).map(str::to_owned);
        if let (300..=399, Some(location)) = (status, location) {
            url = join(&uri, &location);
            // A 303, or a 301 or 302 after a POST, goes on as a GET, as browsers do.
            if status == 303 || ((status == 301 || status == 302) && method == "POST") {
                method = "GET".into();
                body.clear();
            }
            continue;
        }

        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned())))
            .collect();
        let mut bytes = Vec::new();
        response
            .into_body()
            .into_reader()
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|err| format!("Couldn't read the response: {err}"))?;
        if bytes.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(format!("The response is larger than {} MB", MAX_RESPONSE_BYTES / (1024 * 1024)));
        }
        return Ok((HttpReply { status, headers }, bytes));
    }
    Err("Too many redirects".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards_cover_subdomains_only() {
        let allowed = vec!["api.example.com".to_string(), "*.cdn.example.com".to_string()];
        assert!(host_allowed("api.example.com", &allowed));
        assert!(host_allowed("API.Example.com.", &allowed));
        assert!(host_allowed("img.cdn.example.com", &allowed));
        assert!(!host_allowed("cdn.example.com", &allowed));
        assert!(!host_allowed("evilcdn.example.com", &allowed));
        assert!(!host_allowed("api.example.com.evil.net", &allowed));
        assert!(!host_allowed("127.0.0.1", &["127.0.0.1".into()]));
    }

    #[test]
    fn private_and_special_addresses_are_not_public() {
        for ip in ["127.0.0.1", "10.0.0.5", "192.168.1.1", "169.254.169.254", "100.64.0.1", "0.0.0.0", "::1", "fd00::1", "fe80::1", "::ffff:192.168.0.1", "64:ff9b::a00:1"] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip} should not be public");
        }
        assert!(is_public_ip("93.184.216.34".parse().unwrap()));
        assert!(is_public_ip("2606:2800:220:1:248:1893:25c8:1946".parse().unwrap()));
    }

    #[test]
    fn redirects_are_checked_like_the_first_link() {
        let allowed = vec!["api.example.com".to_string()];
        let base: Uri = "https://api.example.com/v1/search?q=a".parse().unwrap();
        assert!(check_url(&join(&base, "/v2/search"), &allowed).is_ok());
        assert!(check_url(&join(&base, "next"), &allowed).is_ok());
        assert!(check_url(&join(&base, "https://other.example.com/"), &allowed).is_err());
        assert!(check_url(&join(&base, "http://api.example.com/"), &allowed).is_err());
        assert!(check_url(&join(&base, "//other.example.com/x"), &allowed).is_err());
        assert!(check_url("https://user:pass@api.example.com/", &allowed).is_err());
    }
}
