//! S/MIME unwrapping (IM-009): work email is routinely signed, and an
//! opaque-signed message hides its entire body and every real attachment
//! inside a single `smime.p7m` — a PKCS#7 wrapper. A user imported one
//! and got a subject line, no body, and "smime.p7m" as the lone
//! attachment. The signature is *not* verified: Marius is a notes tool
//! importing mail its owner already trusts, and says so in frontmatter.

use cms::content_info::ContentInfo;
use cms::signed_data::SignedData;
use der::asn1::{ObjectIdentifier, OctetStringRef};
use der::{Decode, Encode};

use crate::Attachment;

const SIGNED_DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.2");
const ENVELOPED_DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.3");

/// What a `.p7m` turned out to hold.
pub enum Smime {
    /// Opaque-signed: the wrapped MIME entity, readable without any key.
    Signed(Vec<u8>),
    /// Encrypted: unreadable without the recipient's private key.
    Encrypted,
    /// Not PKCS#7 at all (or a shape this does not understand).
    NotSmime,
}

/// Peel a PKCS#7 wrapper. Signed-data yields the inner MIME bytes;
/// enveloped-data is named for what it is rather than imported as noise.
///
/// Outlook's CryptoAPI writes BER with indefinite lengths and chunked
/// octet strings; the strict DER parser rejects those, so anything that
/// fails as DER is normalised first — openssl-made fixtures and real
/// Outlook wrappers both have to pass.
pub fn unwrap(bytes: &[u8]) -> Smime {
    let normalised;
    let der_bytes: &[u8] = match ContentInfo::from_der(bytes) {
        Ok(_) => bytes,
        Err(_) => match ber_to_der(bytes) {
            Some(converted) => {
                normalised = converted;
                &normalised
            }
            None => return Smime::NotSmime,
        },
    };
    let Ok(info) = ContentInfo::from_der(der_bytes) else {
        return Smime::NotSmime;
    };
    if info.content_type == ENVELOPED_DATA {
        return Smime::Encrypted;
    }
    if info.content_type != SIGNED_DATA {
        return Smime::NotSmime;
    }
    let Ok(signed) = info
        .content
        .to_der()
        .and_then(|der| SignedData::from_der(&der))
    else {
        return Smime::NotSmime;
    };
    let Some(content) = signed.encap_content_info.econtent else {
        // A detached signature carries no content; the message's real
        // parts are elsewhere and the caller already has them.
        return Smime::NotSmime;
    };
    match content.decode_as::<OctetStringRef>() {
        Ok(octets) => Smime::Signed(octets.as_bytes().to_vec()),
        Err(_) => Smime::NotSmime,
    }
}

/// What unwrapping did to a message's body and attachments.
pub(crate) enum Unwrapped {
    Signed {
        body_markdown: String,
        attachments: Vec<Attachment>,
    },
    Encrypted,
}

/// If the message's real content hides in a `.p7m` attachment, pull it
/// out: the inner MIME's body (HTML preferred) and its real attachments
/// replace the wrapper. `None` means there was nothing to unwrap.
pub(crate) fn unwrap_wrapped_mime(attachments: &[Attachment]) -> Option<(usize, Unwrapped)> {
    // Probe by content, not filename: the wrapper is usually named
    // smime.p7m, but nothing guarantees it, and probing a handful of
    // attachments costs nothing.
    for (position, attachment) in attachments.iter().enumerate() {
        match unwrap(&attachment.bytes) {
            Smime::Signed(inner) => {
                let (body_markdown, attachments) = crate::email::body_and_attachments(&inner)?;
                return Some((
                    position,
                    Unwrapped::Signed {
                        body_markdown,
                        attachments,
                    },
                ));
            }
            Smime::Encrypted => return Some((position, Unwrapped::Encrypted)),
            Smime::NotSmime => {}
        }
    }
    None
}

/// Re-encode BER as DER: indefinite lengths become definite, and a
/// constructed OCTET STRING's chunks are concatenated into one primitive
/// string (which is also what frees `decode_as::<OctetStringRef>` above
/// from ever meeting a constructed one).
fn ber_to_der(input: &[u8]) -> Option<Vec<u8>> {
    let (out, _used) = ber_node(input)?;
    Some(out)
}

/// One TLV, re-encoded; returns the bytes consumed from the input.
fn ber_node(b: &[u8]) -> Option<(Vec<u8>, usize)> {
    let tag = *b.first()?;
    if tag & 0x1F == 0x1F {
        return None; // high tag numbers do not occur in CMS
    }
    let mut i = 1;
    let first = *b.get(i)?;
    i += 1;
    let constructed = tag & 0x20 != 0;

    // Gather the content, child by child for the indefinite form.
    let content: Vec<u8>;
    if first == 0x80 {
        if !constructed {
            return None;
        }
        let mut children = Vec::new();
        loop {
            if b.get(i..i + 2)? == [0x00, 0x00] {
                i += 2;
                break;
            }
            let (child, used) = ber_node(&b[i..])?;
            children.extend_from_slice(&child);
            i += used;
        }
        content = children;
    } else {
        let length = if first & 0x80 == 0 {
            first as usize
        } else {
            let n = (first & 0x7F) as usize;
            if n == 0 || n > 4 {
                return None;
            }
            let mut v = 0usize;
            for _ in 0..n {
                v = (v << 8) | *b.get(i)? as usize;
                i += 1;
            }
            v
        };
        let raw = b.get(i..i + length)?;
        i += length;
        if constructed {
            // Re-encode the children so nested indefinite forms flatten.
            let mut children = Vec::new();
            let mut j = 0;
            while j < raw.len() {
                let (child, used) = ber_node(&raw[j..])?;
                children.extend_from_slice(&child);
                j += used;
            }
            content = children;
        } else {
            content = raw.to_vec();
        }
    }

    // A constructed string type carries its real bytes in primitive
    // chunks: concatenate them into the primitive form DER demands.
    // UNIVERSAL class only — a context-specific [3] or [4] (class bits
    // set) shares the low tag number and must stay structural.
    let (tag, content) = if constructed && tag & 0xC0 == 0 && matches!(tag & 0x1F, 0x03 | 0x04) {
        let mut merged = Vec::new();
        let mut j = 0;
        while j < content.len() {
            let (chunk, used) = ber_node(&content[j..])?;
            // chunk is already DER: strip its tag + length to get bytes.
            let (_, inner_at) = der_header(&chunk)?;
            merged.extend_from_slice(&chunk[inner_at..]);
            j += used;
        }
        (tag & !0x20, merged)
    } else {
        (tag, content)
    };

    let mut out = vec![tag];
    let n = content.len();
    if n < 0x80 {
        out.push(n as u8);
    } else {
        let bytes = n.to_be_bytes();
        let skip = bytes.iter().take_while(|&&x| x == 0).count();
        out.push(0x80 | (bytes.len() - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(&content);
    Some((out, i))
}

/// Where a DER node's content starts (after tag and length).
fn der_header(b: &[u8]) -> Option<(usize, usize)> {
    let first = *b.get(1)?;
    if first & 0x80 == 0 {
        Some((first as usize, 2))
    } else {
        let n = (first & 0x7F) as usize;
        let mut v = 0usize;
        for k in 0..n {
            v = (v << 8) | *b.get(2 + k)? as usize;
        }
        Some((v, 2 + n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIGNED: &[u8] = include_bytes!("../tests/fixtures/signed.p7m");
    const SIGNED_BER: &[u8] = include_bytes!("../tests/fixtures/signed-ber.p7m");
    const ENCRYPTED: &[u8] = include_bytes!("../tests/fixtures/encrypted.p7m");

    #[test]
    fn a_signed_p7m_yields_the_inner_mime_and_an_encrypted_one_is_named() {
        match unwrap(SIGNED) {
            Smime::Signed(inner) => {
                let text = String::from_utf8_lossy(&inner);
                assert!(text.contains("dredging schedule"), "{text}");
                assert!(text.contains("schedule.csv"), "{text}");
            }
            _ => panic!("signed fixture did not unwrap"),
        }
        assert!(matches!(unwrap(ENCRYPTED), Smime::Encrypted));
        assert!(matches!(unwrap(b"not asn1"), Smime::NotSmime));
    }

    #[test]
    fn outlooks_indefinite_length_ber_unwraps_too() {
        // CryptoAPI writes indefinite lengths and chunked octet strings;
        // the fixture is the DER one mechanically re-encoded that way.
        match unwrap(SIGNED_BER) {
            Smime::Signed(inner) => {
                let text = String::from_utf8_lossy(&inner);
                assert!(text.contains("Nine metres by March"), "{text}");
            }
            _ => panic!("BER wrapper did not unwrap"),
        }
    }

    #[test]
    fn the_wrapper_is_replaced_by_what_it_hid() {
        let wrapped = vec![Attachment {
            file_name: "smime.p7m".into(),
            bytes: SIGNED.to_vec(),
        }];
        let Some((
            0,
            Unwrapped::Signed {
                body_markdown,
                attachments,
            },
        )) = unwrap_wrapped_mime(&wrapped)
        else {
            panic!("did not unwrap");
        };
        assert!(
            body_markdown.contains("Nine metres by March"),
            "{body_markdown}"
        );
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].file_name, "schedule.csv");
    }
}
