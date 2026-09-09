//! HTTP fetching (pure-Rust TLS via rustls) and mirror/metalink resolution.

use std::sync::Arc;
use std::time::Duration;

use crate::RepoError;
use rum_config::{Repo, RepoSource};

/// A reusable HTTP client. Holds a connection-pooling agent. Cheap to clone
/// (the agent is `Arc`-backed) and safe to share across threads.
#[derive(Clone)]
pub struct Http {
    agent: ureq::Agent,
}

impl Default for Http {
    fn default() -> Self {
        Self::new()
    }
}

impl Http {
    pub fn new() -> Self {
        let agent = base_builder().build();
        Http { agent }
    }

    /// Build an HTTP client honouring a repo's TLS settings (client
    /// certificate for mutual-TLS repos like Red Hat RHUI, and an extra CA).
    /// Falls back to the default client when no client cert is configured.
    pub fn for_repo(repo: &Repo) -> Result<Self, RepoError> {
        let (Some(cert), Some(key)) = (&repo.sslclientcert, &repo.sslclientkey) else {
            return Ok(Self::new());
        };
        let config = client_cert_config(cert, key, repo.sslcacert.as_deref())?;
        let agent = base_builder().tls_config(Arc::new(config)).build();
        Ok(Http { agent })
    }

    /// GET a URL and return the raw body bytes.
    pub fn get_bytes(&self, url: &str) -> Result<Vec<u8>, RepoError> {
        let resp = self.agent.get(url).call().map_err(|e| RepoError::Http {
            url: url.to_string(),
            source: Box::new(e),
        })?;
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut resp.into_reader(), &mut buf).map_err(|e| {
            RepoError::Http {
                url: url.to_string(),
                source: Box::new(e),
            }
        })?;
        Ok(buf)
    }

    fn get_text(&self, url: &str) -> Result<String, RepoError> {
        let bytes = self.get_bytes(url)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Resolve a repo source into a list of candidate base URLs (each is a
    /// directory under which `repodata/repomd.xml` is expected), best first.
    pub fn resolve_baseurls(&self, source: &RepoSource) -> Result<Vec<String>, RepoError> {
        match source {
            RepoSource::BaseUrls(urls) => Ok(urls.iter().map(|u| trim_trailing_slash(u)).collect()),
            RepoSource::MirrorList(url) => {
                let text = self.get_text(url)?;
                let mut urls = parse_mirrorlist(&text);
                // Red Hat RHUI's Pulp "mirror" endpoint is itself a working
                // base URL (repodata lives directly under it), even though the
                // URL it lists in its body may return 403. Add the mirrorlist
                // URL as a last-resort base, tried after the advertised
                // mirrors (so ordinary Fedora-style mirrorlists are unchanged).
                urls.push(trim_trailing_slash(url));
                Ok(urls)
            }
            RepoSource::MetaLink(url) => {
                let text = self.get_text(url)?;
                let urls = parse_metalink_baseurls(&text);
                if urls.is_empty() {
                    return Err(RepoError::NoMirrors(url.clone()));
                }
                Ok(urls)
            }
        }
    }
}

fn base_builder() -> ureq::AgentBuilder {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(60))
        .user_agent(concat!("rum/", env!("CARGO_PKG_VERSION")))
}

/// Build a rustls client config with a client certificate (mutual TLS) and,
/// optionally, an extra CA bundle merged with the webpki root store.
fn client_cert_config(
    cert_path: &str,
    key_path: &str,
    cacert_path: Option<&str>,
) -> Result<rustls::ClientConfig, RepoError> {
    let read = |p: &str| -> Result<Vec<u8>, RepoError> {
        std::fs::read(p).map_err(|source| RepoError::Io {
            path: p.into(),
            source,
        })
    };

    // Root store: public roots plus any extra CA (e.g. Red Hat's CDN chain).
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(ca) = cacert_path {
        let ca_bytes = read(ca)?;
        for cert in rustls_pemfile::certs(&mut ca_bytes.as_slice()).flatten() {
            let _ = roots.add(cert);
        }
    }

    // Client certificate chain + private key.
    let cert_bytes = read(cert_path)?;
    let chain: Vec<_> = rustls_pemfile::certs(&mut cert_bytes.as_slice())
        .flatten()
        .collect();
    let key_bytes = read(key_path)?;
    let key = rustls_pemfile::private_key(&mut key_bytes.as_slice())
        .ok()
        .flatten()
        .ok_or_else(|| RepoError::Tls(format!("no private key in {key_path}")))?;

    // Build with an explicit ring provider so we don't depend on a process
    // default crypto provider being installed first.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| RepoError::Tls(e.to_string()))?
        .with_root_certificates(roots)
        .with_client_auth_cert(chain, key)
        .map_err(|e| RepoError::Tls(e.to_string()))
}

/// Detect the current AWS region via IMDSv2 (used to substitute the literal
/// `REGION` token in Red Hat RHUI repo URLs, as the `amazon-id` dnf plugin
/// does). Returns `None` off-EC2 or on any failure.
pub fn detect_aws_region() -> Option<String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_millis(500))
        .timeout_read(Duration::from_millis(500))
        .build();
    let token = agent
        .put("http://169.254.169.254/latest/api/token")
        .set("X-aws-ec2-metadata-token-ttl-seconds", "60")
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let region = agent
        .get("http://169.254.169.254/latest/meta-data/placement/region")
        .set("X-aws-ec2-metadata-token", &token)
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let region = region.trim().to_string();
    (!region.is_empty()).then_some(region)
}

fn trim_trailing_slash(u: &str) -> String {
    u.trim_end_matches('/').to_string()
}

/// A mirrorlist is plain text: one base URL per line, `#` comments, blanks.
fn parse_mirrorlist(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter(|l| {
            l.starts_with("http://") || l.starts_with("https://") || l.starts_with("file://")
        })
        .map(trim_trailing_slash)
        .collect()
}

/// A metalink points at `repodata/repomd.xml` directly with multiple `<url>`
/// entries; we strip that suffix to recover each mirror's base URL.
fn parse_metalink_baseurls(text: &str) -> Vec<String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);
    let mut out = Vec::new();
    let mut in_url = false;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"url" => in_url = true,
            Ok(Event::End(e)) if e.local_name().as_ref() == b"url" => in_url = false,
            Ok(Event::Text(t)) if in_url => {
                let full = t.unescape().unwrap_or_default().to_string();
                if let Some(base) = full.strip_suffix("/repodata/repomd.xml") {
                    out.push(base.to_string());
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mirrorlist() {
        let text = "# comment\n\nhttps://a.example/repo/\nhttp://b.example/repo\ngarbage line\n";
        let urls = parse_mirrorlist(text);
        assert_eq!(
            urls,
            vec!["https://a.example/repo", "http://b.example/repo"]
        );
    }

    #[test]
    fn parses_metalink() {
        let xml = r#"<metalink><files><file name="repomd.xml"><resources>
            <url protocol="https">https://m1.example/os/repodata/repomd.xml</url>
            <url protocol="https">https://m2.example/os/repodata/repomd.xml</url>
        </resources></file></files></metalink>"#;
        let urls = parse_metalink_baseurls(xml);
        assert_eq!(urls, vec!["https://m1.example/os", "https://m2.example/os"]);
    }
}
