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
pub fn unwrap(bytes: &[u8]) -> Smime {
    let Ok(info) = ContentInfo::from_der(bytes) else {
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
    let position = attachments.iter().position(|a| {
        let name = a.file_name.to_ascii_lowercase();
        name.ends_with(".p7m")
    })?;
    match unwrap(&attachments[position].bytes) {
        Smime::Signed(inner) => {
            let (body_markdown, attachments) = crate::email::body_and_attachments(&inner)?;
            Some((
                position,
                Unwrapped::Signed {
                    body_markdown,
                    attachments,
                },
            ))
        }
        Smime::Encrypted => Some((position, Unwrapped::Encrypted)),
        Smime::NotSmime => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIGNED: &[u8] = include_bytes!("../tests/fixtures/signed.p7m");
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
