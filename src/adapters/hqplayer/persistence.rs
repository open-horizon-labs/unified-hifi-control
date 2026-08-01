//! Safe manipulation of HQPlayer Embedded settings archives.
//!
//! This module deliberately has no network code. It validates a fresh daemon backup, resolves a
//! named snapshot, and produces both the intended restore and a rollback restore. Keeping this pure
//! makes the destructive boundary testable without an HQPlayer daemon.

use anyhow::{anyhow, Context, Result};
use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};
use std::collections::HashSet;
use std::io::{Cursor, Read, Write};
use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

const MAX_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
const MAX_MEMBERS: usize = 512;
const MAX_MEMBER_BYTES: u64 = 16 * 1024 * 1024;
const MAX_EXPANDED_BYTES: u64 = 128 * 1024 * 1024;
const MAX_EXPANSION_RATIO: u64 = 200;

#[derive(Debug)]
pub(super) struct ProfileRestorePlan {
    pub restore_archive: Vec<u8>,
    pub rollback_archive: Vec<u8>,
    pub intended_xml: Vec<u8>,
    pub rollback_xml: Vec<u8>,
}

pub(super) fn prepare_profile_restore(
    backup: &[u8],
    profile: &str,
    active_profile: Option<&str>,
) -> Result<ProfileRestorePlan> {
    if profile.is_empty() || profile == "[default]" || profile.eq_ignore_ascii_case("default") {
        return Err(anyhow!("a named HQPlayer profile is required"));
    }
    if profile.contains('/') || profile.contains('\\') || profile == "." || profile == ".." {
        return Err(anyhow!("unsafe HQPlayer profile name"));
    }

    let members = validated_members(backup)?;
    let snapshot_name = format!("data/cfgs/{profile}.xml");
    let intended_xml = member_bytes(backup, &snapshot_name)
        .with_context(|| format!("profile {profile:?} is absent from the fresh backup"))?;
    validate_config_xml(&intended_xml).context("selected profile XML is invalid")?;

    let working_name = resolve_working_member(&members, active_profile)?;
    let rollback_xml = member_bytes(backup, &working_name)
        .context("fresh backup has no readable working configuration")?;
    validate_config_xml(&rollback_xml).context("working configuration XML is invalid")?;

    Ok(ProfileRestorePlan {
        restore_archive: rewrite_working(backup, &intended_xml)?,
        rollback_archive: rewrite_working(backup, &rollback_xml)?,
        intended_xml,
        rollback_xml,
    })
}

pub(super) fn working_config(backup: &[u8], active_profile: Option<&str>) -> Result<Vec<u8>> {
    let members = validated_members(backup)?;
    let name = resolve_working_member(&members, active_profile)?;
    member_bytes(backup, &name).context("fresh backup has no readable working configuration")
}

pub(super) fn semantically_equal(left: &[u8], right: &[u8]) -> Result<bool> {
    Ok(canonical_xml(left)? == canonical_xml(right)?)
}

fn validated_members(backup: &[u8]) -> Result<Vec<String>> {
    if backup.is_empty() || backup.len() > MAX_ARCHIVE_BYTES {
        return Err(anyhow!("backup archive size {} is unsafe", backup.len()));
    }
    let mut archive =
        ZipArchive::new(Cursor::new(backup)).context("backup is not a readable ZIP")?;
    if archive.is_empty() || archive.len() > MAX_MEMBERS {
        return Err(anyhow!(
            "backup archive member count {} is unsafe",
            archive.len()
        ));
    }
    let mut names = Vec::with_capacity(archive.len());
    let mut seen = HashSet::new();
    let mut expanded = 0u64;
    for index in 0..archive.len() {
        let file = archive.by_index(index)?;
        let name = file.name().to_string();
        if file.enclosed_name().is_none() || name.starts_with('/') || name.contains('\\') {
            return Err(anyhow!("backup contains unsafe member path {name:?}"));
        }
        if !seen.insert(name.clone()) {
            return Err(anyhow!("backup contains duplicate member {name:?}"));
        }
        if file
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err(anyhow!("backup contains symbolic link {name:?}"));
        }
        if file.size() > MAX_MEMBER_BYTES {
            return Err(anyhow!("backup member {name:?} is oversized"));
        }
        expanded = expanded.saturating_add(file.size());
        if expanded > MAX_EXPANDED_BYTES {
            return Err(anyhow!("backup expanded size is unsafe"));
        }
        let compressed = file.compressed_size().max(1);
        if file.size() / compressed > MAX_EXPANSION_RATIO {
            return Err(anyhow!(
                "backup member {name:?} has an unsafe expansion ratio"
            ));
        }
        names.push(name);
    }
    Ok(names)
}

fn resolve_working_member(names: &[String], active_profile: Option<&str>) -> Result<String> {
    if names.iter().any(|name| name == "hqplayerd.xml") {
        return Ok("hqplayerd.xml".to_string());
    }
    let roots: Vec<&String> = names
        .iter()
        .filter(|name| !name.contains('/') && name.ends_with(".xml"))
        .collect();
    if let Some(active) = active_profile.filter(|active| !active.is_empty()) {
        let named = format!("{active}.xml");
        if roots.iter().any(|root| root.as_str() == named) {
            return Ok(named);
        }
        return Err(anyhow!(
            "fresh backup does not contain the ConfigurationGet working member {named:?}"
        ));
    }
    match roots.as_slice() {
        [only] => Ok((*only).clone()),
        [] => Err(anyhow!("fresh backup contains no working configuration")),
        _ => Err(anyhow!(
            "fresh backup has multiple root XML members and no hqplayerd.xml"
        )),
    }
}

fn member_bytes(backup: &[u8], name: &str) -> Result<Vec<u8>> {
    let mut archive = ZipArchive::new(Cursor::new(backup))?;
    let mut file = archive.by_name(name)?;
    let mut bytes = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn rewrite_working(backup: &[u8], xml: &[u8]) -> Result<Vec<u8>> {
    let input = ZipArchive::new(Cursor::new(backup))?;
    let mut output = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut input = input;
    let mut replaced = false;
    for index in 0..input.len() {
        let mut file = input.by_index(index)?;
        let name = file.name().to_string();
        if name == "hqplayerd.xml" {
            output.start_file(&name, options)?;
            output.write_all(xml)?;
            replaced = true;
        } else if file.is_dir() {
            output.add_directory(&name, options)?;
        } else {
            output.start_file(&name, options)?;
            std::io::copy(&mut file, &mut output)?;
        }
    }
    if !replaced {
        output.start_file("hqplayerd.xml", options)?;
        output.write_all(xml)?;
    }
    Ok(output.finish()?.into_inner())
}

fn validate_config_xml(xml: &[u8]) -> Result<()> {
    let canonical = canonical_xml(xml)?;
    if canonical.first().map(String::as_str) != Some("S:hqplayerd") {
        return Err(anyhow!("configuration root is not <hqplayerd>"));
    }
    Ok(())
}

fn canonical_xml(xml: &[u8]) -> Result<Vec<String>> {
    let mut reader = Reader::from_reader(Cursor::new(xml));
    reader.config_mut().check_end_names = true;
    let mut buf = Vec::new();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut roots = 0usize;
    loop {
        match reader.read_event_into(&mut buf).context("malformed XML")? {
            Event::Start(start) => {
                if depth == 0 {
                    roots += 1;
                }
                depth += 1;
                let name = String::from_utf8_lossy(start.name().as_ref()).into_owned();
                let mut attrs = Vec::new();
                for attr in start.attributes().with_checks(true) {
                    let attr = attr.context("invalid XML attribute")?;
                    attrs.push((
                        String::from_utf8_lossy(attr.key.as_ref()).into_owned(),
                        attr.decoded_and_normalized_value(
                            XmlVersion::Explicit1_0,
                            reader.decoder(),
                        )?
                        .into_owned(),
                    ));
                }
                attrs.sort();
                out.push(format!("S:{name}"));
                out.extend(attrs.into_iter().map(|(k, v)| format!("A:{k}={v}")));
            }
            Event::Empty(start) => {
                if depth == 0 {
                    roots += 1;
                }
                let name = String::from_utf8_lossy(start.name().as_ref()).into_owned();
                let mut attrs = Vec::new();
                for attr in start.attributes().with_checks(true) {
                    let attr = attr.context("invalid XML attribute")?;
                    attrs.push((
                        String::from_utf8_lossy(attr.key.as_ref()).into_owned(),
                        attr.decoded_and_normalized_value(
                            XmlVersion::Explicit1_0,
                            reader.decoder(),
                        )?
                        .into_owned(),
                    ));
                }
                attrs.sort();
                out.push(format!("S:{name}"));
                out.extend(attrs.into_iter().map(|(k, v)| format!("A:{k}={v}")));
                out.push(format!("E:{name}"));
            }
            Event::End(end) => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| anyhow!("XML closes outside the document root"))?;
                out.push(format!(
                    "E:{}",
                    String::from_utf8_lossy(end.name().as_ref())
                ));
            }
            Event::Text(text) => {
                let decoded = text.xml10_content()?;
                let value = quick_xml::escape::unescape(&decoded)?.trim().to_string();
                if !value.is_empty() {
                    out.push(format!("T:{value}"));
                }
            }
            Event::CData(text) => out.push(format!("T:{}", String::from_utf8_lossy(&text))),
            Event::Comment(text) => {
                let value = String::from_utf8_lossy(&text).trim().to_string();
                if !value.is_empty() {
                    out.push(format!("C:{value}"));
                }
            }
            Event::Decl(_) | Event::PI(_) | Event::DocType(_) => {}
            Event::GeneralRef(reference) => {
                return Err(anyhow!(
                    "configuration contains unsupported entity reference &{};",
                    String::from_utf8_lossy(&reference)
                ));
            }
            Event::Eof => break,
        }
        buf.clear();
    }
    if roots != 1 || depth != 0 {
        return Err(anyhow!(
            "configuration must contain exactly one complete root"
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        for (name, body) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(body).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn profile_restore_is_surgical_and_carries_a_rollback_working_file() {
        let old = br#"<?xml version="1.0"?><hqplayerd><engine cuda="0"/></hqplayerd>"#;
        let target = br#"<?xml version="1.0"?><hqplayerd><engine cuda="1"/></hqplayerd>"#;
        let backup = archive(&[
            ("hqplayerd.xml", old),
            ("data/cfgs/GPU.xml", target),
            ("data/keep", b"opaque"),
        ]);
        let plan = prepare_profile_restore(&backup, "GPU", None).unwrap();
        assert!(semantically_equal(
            &working_config(&plan.restore_archive, None).unwrap(),
            target
        )
        .unwrap());
        assert!(
            semantically_equal(&working_config(&plan.rollback_archive, None).unwrap(), old)
                .unwrap()
        );
        assert_eq!(
            member_bytes(&plan.restore_archive, "data/keep").unwrap(),
            b"opaque"
        );
    }

    #[test]
    fn refuses_empty_traversal_and_ambiguous_working_archives() {
        assert!(prepare_profile_restore(&[], "GPU", None).is_err());
        let traversal = archive(&[("../hqplayerd.xml", b"x"), ("data/cfgs/GPU.xml", b"x")]);
        assert!(prepare_profile_restore(&traversal, "GPU", None).is_err());
        let ambiguous = archive(&[
            ("A.xml", b"<hqplayerd/>"),
            ("B.xml", b"<hqplayerd/>"),
            ("data/cfgs/GPU.xml", b"<hqplayerd/>"),
        ]);
        assert!(prepare_profile_restore(&ambiguous, "GPU", None).is_err());
        let named = archive(&[
            (
                "Speakers.xml",
                b"<hqplayerd><title value=\"speakers\"/></hqplayerd>",
            ),
            (
                "Headphones.xml",
                b"<hqplayerd><title value=\"headphones\"/></hqplayerd>",
            ),
            ("data/cfgs/GPU.xml", b"<hqplayerd/>"),
        ]);
        let plan = prepare_profile_restore(&named, "GPU", Some("Speakers")).unwrap();
        assert!(String::from_utf8_lossy(&plan.rollback_xml).contains("speakers"));
        assert!(prepare_profile_restore(&named, "GPU", Some("Missing")).is_err());
    }

    #[test]
    fn semantic_readback_ignores_formatting_and_attribute_order_but_not_values() {
        let a = br#"<hqplayerd><engine cuda="1" multicore="auto"/></hqplayerd>"#;
        let b = br#"<hqplayerd>
  <engine multicore="auto" cuda="1" />
</hqplayerd>"#;
        let c = br#"<hqplayerd><engine cuda="0" multicore="auto"/></hqplayerd>"#;
        assert!(semantically_equal(a, b).unwrap());
        assert!(!semantically_equal(a, c).unwrap());
        assert!(semantically_equal(b"<hqplayerd/><other/>", b"<hqplayerd/>").is_err());
    }
}
