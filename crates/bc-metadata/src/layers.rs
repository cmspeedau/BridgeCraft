//! The metadata layers of one file, merged in the documented priority order.
//!
//! A single file can state the same property in up to four places: EXIF,
//! IPTC-IIM, an XMP packet embedded in the container, and an `.xmp` sidecar
//! beside it. `docs/design/0005-structured-metadata-model.md` section 3 fixes
//! the priority as **sidecar > embedded XMP > IPTC > EXIF**, applied property
//! by property rather than per source.
//!
//! This module is the only place that knows that order. The metadata panel and
//! the export pipeline both call [`read_file`], so a file cannot mean one
//! thing in the UI and another in an export. That split is what this replaces:
//! the chain used to live in `bc-ui::metadata_edit`, and the export path grew
//! its own partial view of EXIF only, which meant tags read by the panel could
//! silently fail to reach an exported file.
//!
//! Clean-room: the layer set and ordering come from the published XMP
//! specification and the container formats' own documentation.

use std::path::Path;

use crate::{Metadata, MetadataError, Value};

/// EXIF properties written through XMP use this namespace URI.
pub const XMP_EXIF_URI: &str = "http://ns.adobe.com/exif/1.0/";
/// TIFF properties written through XMP use this namespace URI.
pub const XMP_TIFF_URI: &str = "http://ns.adobe.com/tiff/1.0/";

/// Every metadata layer of `path`, merged low priority to high.
///
/// Reading never fails because one layer is broken: a corrupt XMP packet is
/// reported by the caller seeing fewer properties, and the file is still
/// browsable, which is what an asset browser has to do with untrusted input.
/// The error is returned only when there is nothing to return at all, which
/// today cannot happen, so callers that only want properties can `unwrap_or`.
pub fn read_file(path: &Path) -> Result<Metadata, MetadataError> {
    let bytes = std::fs::read(path)?;
    read_bytes(&bytes, path)
}

/// The same chain for bytes already in memory, so a caller that has the file
/// open does not read it twice. `path` is used only to locate the sidecar.
pub fn read_bytes(bytes: &[u8], path: &Path) -> Result<Metadata, MetadataError> {
    let mut meta = Metadata::new();

    // 1. EXIF (lowest priority).
    if let Ok(exif) = crate::exif::read_exif(bytes) {
        merge_properties(&mut meta, &exif);
    }

    // 2. IPTC-IIM, wherever it is parked.
    if let Ok(iptc) = crate::iptc::read_iptc(bytes) {
        merge_properties(&mut meta, &iptc);
    }

    // 3. An XMP packet embedded in the container.
    if let Some(packet) = crate::embedded::find_xmp_packet(bytes)? {
        match crate::xmp::parse_xmp(&packet) {
            Ok(parsed) => merge_xmp(&mut meta, &parsed),
            Err(error) => {
                // A packet we cannot read must not look like "no metadata":
                // say so loudly enough that a caller can surface it.
                tracing::debug!("layers: skipping unreadable embedded packet: {error}");
            }
        }
    }

    // 4. The sidecar, which wins.
    let sidecar = crate::xmp::sidecar_path(path);
    if sidecar.exists() {
        match crate::xmp::read_xmp_file(&sidecar) {
            Ok(parsed) => merge_xmp(&mut meta, &parsed),
            Err(error) => {
                tracing::debug!("layers: sidecar {} unreadable: {error}", sidecar.display());
            }
        }
    }

    Ok(meta)
}

/// Copy every property of `source`, replacing same-named ones. The higher
/// layer's value wins; a property it does not carry is left as it was.
pub fn merge_properties(target: &mut Metadata, source: &Metadata) {
    for (name, value) in source.properties() {
        target.set(name.namespace.clone(), name.name.clone(), value.clone());
    }
}

/// Merge an XMP packet, plus one projection step: EXIF and TIFF properties
/// carried as XMP are also written into the short `"exif"` namespace, so a
/// field list that reads `exif/Artist` sees the value however it was stored.
pub fn merge_xmp(target: &mut Metadata, packet: &Metadata) {
    for (name, value) in packet.properties() {
        target.set(name.namespace.clone(), name.name.clone(), value.clone());
        if name.namespace == XMP_EXIF_URI || name.namespace == XMP_TIFF_URI {
            if let Value::Text(text) = value {
                target.set_text(crate::NS_EXIF, name.name.clone(), text.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NS_DC, NS_EXIF, NS_PHOTOSHOP, NS_XMP};

    fn jpeg_with_xmp(packet: &[u8]) -> Vec<u8> {
        let mut body = crate::embedded::XMP_JPEG_SIGNATURE.to_vec();
        body.extend_from_slice(packet);
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
        out.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&body);
        out.extend_from_slice(&[0xFF, 0xDA, 0x04, 0x00, 0x01]);
        out.extend_from_slice(&[0xFF, 0xD9]);
        out
    }

    fn packet(rating: &str, artist: &str) -> Vec<u8> {
        format!(
            "<?xpacket begin=''?><x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'>\
             <rdf:Description rdf:about='' xmlns:xmp='{NS_XMP_URI}' xmlns:exif='{XMP_EXIF_URI}'>\
             <xmp:Rating>{rating}</xmp:Rating><exif:Artist>{artist}</exif:Artist>\
             </rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end='w'?>",
            NS_XMP_URI = "http://ns.adobe.com/xap/1.0/",
            XMP_EXIF_URI = XMP_EXIF_URI
        )
        .into_bytes()
    }

    #[test]
    fn the_sidecar_outranks_the_embedded_packet_property_by_property() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("layered.jpg");
        std::fs::write(&file, jpeg_with_xmp(&packet("3", "From The Packet"))).unwrap();

        // The sidecar claims only the rating, so the Artist must survive from
        // the lower layer rather than being wiped by a higher one.
        let mut side = Metadata::new();
        side.set(NS_XMP, "Rating", Value::Integer(5));
        crate::xmp::write_xmp_file(&crate::xmp::sidecar_path(&file), &side).unwrap();

        let meta = read_file(&file).unwrap();
        assert_eq!(meta.rating().stars(), 5, "sidecar wins the rating");
        assert_eq!(
            meta.text(NS_EXIF, "Artist").map(str::to_string),
            Some("From The Packet".to_string()),
            "the sidecar said nothing about Artist, so the packet's value stands"
        );
    }

    #[test]
    fn exif_namespaced_xmp_properties_reach_the_short_exif_view() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("proj.jpg");
        std::fs::write(&file, jpeg_with_xmp(&packet("2", "Projected Artist"))).unwrap();
        let meta = read_file(&file).unwrap();
        assert_eq!(
            meta.text(NS_EXIF, "Artist"),
            Some("Projected Artist"),
            "the panel's field list reads the short namespace"
        );
        assert_eq!(meta.text(XMP_EXIF_URI, "Artist"), Some("Projected Artist"));
    }

    #[test]
    fn an_unreadable_embedded_packet_does_not_hide_the_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("broken.jpg");
        // Not XML at all, inside a real APP1 xap segment.
        std::fs::write(
            &file,
            jpeg_with_xmp(b"this is not an xmp packet, and never will be"),
        )
        .unwrap();
        let mut side = Metadata::new();
        side.set_text(NS_DC, "subject", "Still Read".to_string());
        side.set(NS_PHOTOSHOP, "Label", Value::Text("Green".to_string()));
        crate::xmp::write_xmp_file(&crate::xmp::sidecar_path(&file), &side).unwrap();

        let meta = read_file(&file).unwrap();
        assert!(
            crate::keywords::subjects(&meta).contains(&"Still Read".to_string()),
            "a broken packet must not cost the user their sidecar keywords"
        );
        assert_eq!(meta.color_label().as_label_str(), Some("Green"));
    }

    #[test]
    fn read_file_reports_a_missing_file_instead_of_inventing_empty_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let error = read_file(&dir.path().join("gone.jpg")).unwrap_err();
        assert!(
            matches!(error, MetadataError::Io(_)),
            "an absent file is an I/O error, not an empty answer: {error:?}"
        );
    }
}
