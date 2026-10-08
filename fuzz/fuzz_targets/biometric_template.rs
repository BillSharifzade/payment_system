//! `Template::from_base64` for every format: accepts exactly what the specification accepts
//! (known format, base64 after trimming, 16..=16384 bytes, the FMR magic for ISO/ANSI), with
//! the same error precedence; agrees with `Template::new` on the decoded bytes; round-trips
//! through `to_base64`; and the hash binds the format.
#![no_main]

use arbitrary::Arbitrary;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use biometric::{BiometricError, Template, TemplateFormat, MAX_TEMPLATE_BYTES, MIN_TEMPLATE_BYTES};
use libfuzzer_sys::fuzz_target;
use sha2::{Digest, Sha256};

#[derive(Arbitrary, Debug)]
enum Format {
    Iso,
    Ansi,
    Raw,
    Other(String),
}

#[derive(Arbitrary, Debug)]
struct Input<'a> {
    format: Format,
    encoded: &'a str,
    bytes: Vec<u8>,
    magic: bool,
    resize: Option<u16>,
    padding: (u8, u8),
}

fn name(f: &Format) -> &str {
    match f {
        Format::Iso => "iso-19794-2",
        Format::Ansi => "ansi-378",
        Format::Raw => "raw",
        Format::Other(s) => s,
    }
}

fn spec(format: &str, encoded: &str) -> Result<(TemplateFormat, Vec<u8>), BiometricError> {
    let f = match format {
        "iso-19794-2" => TemplateFormat::Iso19794_2,
        "ansi-378" => TemplateFormat::Ansi378,
        "raw" => TemplateFormat::Raw,
        other => return Err(BiometricError::UnsupportedFormat(other.to_string())),
    };
    let bytes = STANDARD
        .decode(encoded.trim())
        .map_err(|_| BiometricError::Base64)?;
    if !(MIN_TEMPLATE_BYTES..=MAX_TEMPLATE_BYTES).contains(&bytes.len()) {
        return Err(BiometricError::TemplateSize {
            min: MIN_TEMPLATE_BYTES,
            max: MAX_TEMPLATE_BYTES,
            got: bytes.len(),
        });
    }
    if f != TemplateFormat::Raw && !bytes.starts_with(b"FMR\0") {
        return Err(BiometricError::TemplateMagic(f.as_str()));
    }
    Ok((f, bytes))
}

fn check(format: &str, encoded: &str) {
    let got = Template::from_base64(format, encoded);
    match (spec(format, encoded), &got) {
        (Ok((f, bytes)), Ok(t)) => {
            assert_eq!((t.format(), t.bytes()), (f, &bytes[..]));
            assert_eq!(TemplateFormat::parse(f.as_str()), Ok(f));
            assert_eq!(
                Template::from_base64(f.as_str(), &t.to_base64()).as_ref(),
                Ok(t)
            );
            let mut h = Sha256::new();
            h.update(f.as_str());
            h.update([0]);
            h.update(&bytes);
            assert_eq!(t.hash(), <[u8; 32]>::from(h.finalize()));
            if f != TemplateFormat::Raw {
                let raw = Template::new(TemplateFormat::Raw, bytes).unwrap();
                assert_ne!(raw.hash(), t.hash(), "the hash must bind the format");
            }
        }
        (want, got) => assert_eq!(
            want.err().as_ref(),
            got.as_ref().err(),
            "{format:?} {encoded:?}"
        ),
    }
}

fuzz_target!(|input: Input| {
    let format = name(&input.format);
    check(format, input.encoded);

    let mut bytes = input.bytes;
    if input.magic {
        bytes.splice(0..0, *b"FMR\0");
    }
    if let Some(n) = input.resize {
        bytes.resize(n as usize, 0x5a);
    }
    let encoded = STANDARD.encode(&bytes);
    let direct = Template::from_base64(format, &encoded);
    if let Ok(f) = TemplateFormat::parse(format) {
        assert_eq!(direct, Template::new(f, bytes.clone()));
    }
    let ws = [" ", "\n", "\t", "\r\n"];
    let padded = format!(
        "{}{encoded}{}",
        ws[input.padding.0 as usize % ws.len()],
        ws[input.padding.1 as usize % ws.len()]
    );
    assert_eq!(Template::from_base64(format, &padded), direct);
    check(format, &padded);
});
