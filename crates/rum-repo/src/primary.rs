//! Streaming parser for `primary.xml` into a list of available packages.
//!
//! We extract the fields needed for listing/searching/info and for locating
//! the RPM to download: name, arch, EVR, summary, size, location, checksum.
//! Dependency data (`provides`/`requires`) lives in `<format>` and is left for
//! the solver milestone.

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::checksum::{Checksum, ChecksumKind};
use crate::RepoError;

/// A package as advertised by a repository (not necessarily installed).
#[derive(Debug, Clone)]
pub struct AvailablePackage {
    pub name: String,
    /// Epoch as advertised; 0 means "no epoch" for display purposes.
    pub epoch: u64,
    pub version: String,
    pub release: String,
    pub arch: String,
    pub summary: String,
    /// Compressed RPM size in bytes (`<size package=...>`).
    pub size: u64,
    /// href relative to the repo base URL.
    pub location: String,
    pub checksum: Checksum,
    /// Which repo this came from (filled in by the caller).
    pub repo_id: String,
}

impl AvailablePackage {
    pub fn evr(&self) -> String {
        if self.epoch == 0 {
            format!("{}-{}", self.version, self.release)
        } else {
            format!("{}:{}-{}", self.epoch, self.version, self.release)
        }
    }
    pub fn name_arch(&self) -> String {
        format!("{}.{}", self.name, self.arch)
    }
    pub fn nevra(&self) -> String {
        format!("{}-{}.{}", self.name, self.evr(), self.arch)
    }
}

/// Parse the decompressed primary.xml, tagging every package with `repo_id`.
pub fn parse(xml: &[u8], repo_id: &str) -> Result<Vec<AvailablePackage>, RepoError> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);

    let mut out = Vec::new();
    let mut buf = Vec::new();

    let mut cur: Option<Builder> = None;
    // Which text-bearing child we're currently capturing.
    let mut field = Field::None;
    // Only capture the top-level package checksum (pkgid="YES"), not the ones
    // nested in <format>.
    let mut depth_in_package = 0i32;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.local_name().as_ref().to_vec();
                match name.as_slice() {
                    b"package" => {
                        cur = Some(Builder::default());
                        depth_in_package = 0;
                    }
                    _ if cur.is_some() => {
                        depth_in_package += 1;
                        if let Some(b) = cur.as_mut() {
                            match name.as_slice() {
                                b"name" => field = Field::Name,
                                b"arch" => field = Field::Arch,
                                b"summary" => field = Field::Summary,
                                b"version" => b.read_version(&e),
                                b"checksum" if depth_in_package == 1 => {
                                    b.pending_cksum_kind = attr_opt(&e, b"type")
                                        .and_then(|s| ChecksumKind::parse(&s));
                                    field = Field::Checksum;
                                }
                                b"location" => b.location = attr(&e, b"href"),
                                b"size" => b.read_size(&e),
                                _ => field = Field::None,
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) if cur.is_some() => {
                // Self-closing attribute-bearing elements.
                let name = e.local_name().as_ref().to_vec();
                if let Some(b) = cur.as_mut() {
                    match name.as_slice() {
                        b"version" => b.read_version(&e),
                        b"location" => b.location = attr(&e, b"href"),
                        b"size" => b.read_size(&e),
                        _ => {}
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if let Some(b) = cur.as_mut() {
                    let text = t.unescape().unwrap_or_default();
                    match field {
                        Field::Name => b.name = text.into_owned(),
                        Field::Arch => b.arch = text.into_owned(),
                        Field::Summary => b.summary = text.into_owned(),
                        Field::Checksum => {
                            if let Some(kind) = b.pending_cksum_kind {
                                b.checksum = Some(Checksum {
                                    kind,
                                    hex: text.into_owned(),
                                });
                            }
                        }
                        Field::None => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = e.local_name().as_ref().to_vec();
                if name.as_slice() == b"package" {
                    if let Some(b) = cur.take() {
                        if let Some(pkg) = b.finish(repo_id) {
                            out.push(pkg);
                        }
                    }
                } else if cur.is_some() {
                    depth_in_package -= 1;
                }
                field = Field::None;
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(RepoError::Xml(format!("primary.xml: {e}"))),
            _ => {}
        }
        buf.clear();
    }

    Ok(out)
}

#[derive(Clone, Copy, PartialEq)]
enum Field {
    None,
    Name,
    Arch,
    Summary,
    Checksum,
}

#[derive(Default)]
struct Builder {
    name: String,
    epoch: u64,
    version: String,
    release: String,
    arch: String,
    summary: String,
    size: u64,
    location: String,
    checksum: Option<Checksum>,
    pending_cksum_kind: Option<ChecksumKind>,
}

impl Builder {
    fn read_version(&mut self, e: &quick_xml::events::BytesStart) {
        self.epoch = attr_opt(e, b"epoch").and_then(|s| s.parse().ok()).unwrap_or(0);
        self.version = attr(e, b"ver");
        self.release = attr(e, b"rel");
    }
    fn read_size(&mut self, e: &quick_xml::events::BytesStart) {
        self.size = attr_opt(e, b"package").and_then(|s| s.parse().ok()).unwrap_or(0);
    }
    fn finish(self, repo_id: &str) -> Option<AvailablePackage> {
        if self.name.is_empty() || self.version.is_empty() || self.location.is_empty() {
            return None;
        }
        Some(AvailablePackage {
            name: self.name,
            epoch: self.epoch,
            version: self.version,
            release: self.release,
            arch: self.arch,
            summary: self.summary,
            size: self.size,
            location: self.location,
            checksum: self.checksum?,
            repo_id: repo_id.to_string(),
        })
    }
}

fn attr(e: &quick_xml::events::BytesStart, key: &[u8]) -> String {
    attr_opt(e, key).unwrap_or_default()
}

fn attr_opt(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == key)
        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_packages() {
        let xml = br#"<?xml version="1.0"?>
        <metadata xmlns="http://linux.duke.edu/metadata/common"
                  xmlns:rpm="http://linux.duke.edu/metadata/rpm" packages="2">
          <package type="rpm">
            <name>bash</name>
            <arch>x86_64</arch>
            <version epoch="0" ver="5.2.15" rel="1.amzn2023"/>
            <checksum type="sha256" pkgid="YES">deadbeef</checksum>
            <summary>The GNU Bourne Again shell</summary>
            <size package="1234" installed="5678" archive="9012"/>
            <location href="Packages/b/bash-5.2.15-1.amzn2023.x86_64.rpm"/>
            <format>
              <rpm:provides><rpm:entry name="bash"/></rpm:provides>
              <checksum>should-not-be-picked</checksum>
            </format>
          </package>
          <package type="rpm">
            <name>zlib</name>
            <arch>x86_64</arch>
            <version epoch="2" ver="1.2.13" rel="3"/>
            <checksum type="sha256" pkgid="YES">cafef00d</checksum>
            <summary>zlib lib</summary>
            <size package="4321"/>
            <location href="Packages/z/zlib-1.2.13-3.x86_64.rpm"/>
          </package>
        </metadata>"#;

        let pkgs = parse(xml, "baseos").unwrap();
        assert_eq!(pkgs.len(), 2);

        let bash = &pkgs[0];
        assert_eq!(bash.name, "bash");
        assert_eq!(bash.evr(), "5.2.15-1.amzn2023");
        assert_eq!(bash.nevra(), "bash-5.2.15-1.amzn2023.x86_64");
        assert_eq!(bash.size, 1234);
        assert_eq!(bash.summary, "The GNU Bourne Again shell");
        assert_eq!(bash.checksum.hex, "deadbeef"); // not the <format> one
        assert_eq!(bash.repo_id, "baseos");

        let zlib = &pkgs[1];
        assert_eq!(zlib.evr(), "2:1.2.13-3");
    }
}
