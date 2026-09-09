//! HTTP fetching (pure-Rust TLS via rustls) and mirror/metalink resolution.

use std::time::Duration;

use crate::RepoError;
use rum_config::RepoSource;

/// A reusable HTTP client. Holds a connection-pooling agent.
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
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .timeout_read(Duration::from_secs(60))
            .user_agent(concat!("rum/", env!("CARGO_PKG_VERSION")))
            .build();
        Http { agent }
    }

    /// GET a URL and return the raw body bytes.
    pub fn get_bytes(&self, url: &str) -> Result<Vec<u8>, RepoError> {
        let resp = self
            .agent
            .get(url)
            .call()
            .map_err(|e| RepoError::Http {
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
                let urls = parse_mirrorlist(&text);
                if urls.is_empty() {
                    return Err(RepoError::NoMirrors(url.clone()));
                }
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

fn trim_trailing_slash(u: &str) -> String {
    u.trim_end_matches('/').to_string()
}

/// A mirrorlist is plain text: one base URL per line, `#` comments, blanks.
fn parse_mirrorlist(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter(|l| l.starts_with("http://") || l.starts_with("https://") || l.starts_with("file://"))
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
        assert_eq!(urls, vec!["https://a.example/repo", "http://b.example/repo"]);
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
