//! A fresh shared-memory challenge proves that a local peer can read a
//! descriptor before any camera pixels are offered. It never runs over TCP/QUIC.
use crate::ring::{PictureIdentity, RawFormat, RawLayout, RingError, RingReader, RingWriter};
use nw_analytics_protocol::decoded::LocalityChallenge;
use std::fs::File;
use std::path::Path;
use subtle::ConstantTimeEq;

pub struct LocalityProbe {
    pub ring: RingWriter,
    expected: String,
}

impl LocalityProbe {
    pub fn create(directory: &Path) -> Result<Self, RingError> {
        let mut ring = RingWriter::create(
            directory,
            RawLayout {
                width: 32,
                height: 1,
                format: RawFormat::Gray8,
                color: None,
            },
        )?;
        let mut value = [0u8; 32];
        getrandom::getrandom(&mut value).map_err(|_| RingError::Random)?;
        ring.write(
            PictureIdentity {
                seq: 0,
                time_us: 0,
                pts_ns: None,
            },
            [&value, &[]],
            [32, 0],
        )?;
        Ok(Self {
            ring,
            expected: hex(&value),
        })
    }
    pub fn offer(&self) -> LocalityChallenge {
        LocalityChallenge {
            ring_id: self.ring.id().into(),
            bytes: self.ring.bytes(),
            handle: None,
        }
    }
    pub fn verify(&self, answer: Option<&str>) -> bool {
        answer.is_some_and(|answer| self.expected.as_bytes().ct_eq(answer.as_bytes()).into())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn answer(file: File, offer: &LocalityChallenge) -> Result<String, RingError> {
    let mut reader = RingReader::open(file, &offer.ring_id, offer.bytes)?;
    let frame = reader.latest().ok_or(RingError::Invalid)?;
    if frame.layout.format != RawFormat::Gray8 || frame.data.len() != 32 {
        return Err(RingError::Invalid);
    }
    Ok(hex(&frame.data))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn locality_requires_reading_the_fresh_descriptor_and_cannot_be_replayed() {
        let dir = std::env::temp_dir().join(format!("nw-locality-{}", std::process::id()));
        let a = LocalityProbe::create(&dir).unwrap();
        let b = LocalityProbe::create(&dir).unwrap();
        let proof = answer(a.ring.descriptor().unwrap(), &a.offer()).unwrap();
        assert!(a.verify(Some(&proof)));
        assert!(!b.verify(Some(&proof)));
        assert!(!a.verify(None));
        assert!(!a.verify(Some("")));
        assert!(!a.verify(Some(&"0".repeat(64))));
        let mut wrong = a.offer();
        wrong.ring_id = b.ring.id().into();
        assert!(answer(a.ring.descriptor().unwrap(), &wrong).is_err());
        let _ = std::fs::remove_dir(dir);
    }
}
