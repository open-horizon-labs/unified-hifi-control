//! Bounded, evidence-first framing for a private NAA trace.
pub mod discovery;
pub mod pcm;
pub mod server;
use std::fmt;
pub const HEADER_LEN: usize = 32;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub mask: u32,
    pub pcm_len: u32,
    pub pos_len: u32,
    pub meta_len: u32,
    pub pic_len: u32,
    pub reserved: [u8; 12],
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFrame {
    pub offset: u64,
    pub header: Header,
    pub pcm: Vec<u8>,
    pub position: Vec<u8>,
    pub metadata: Vec<u8>,
    pub picture: Vec<u8>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    Control { offset: u64, bytes: Vec<u8> },
    Audio(AudioFrame),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub offset: u64,
    pub kind: ErrorKind,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidSampleBytes,
    InvalidControl,
    ControlTooLong,
    FrameTooLarge,
    Truncated { needed: usize, available: usize },
    ArithmeticOverflow,
}
impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "parse error at byte {}: {:?}", self.offset, self.kind)
    }
}
impl std::error::Error for ParseError {}
pub struct Parser {
    sample_bytes: usize,
    max_frame: usize,
    max_control: usize,
    buffer: Vec<u8>,
    offset: u64,
    terminal: Option<ParseError>,
    control_scan: usize,
}
impl Parser {
    pub fn new(
        sample_bytes: usize,
        max_frame: usize,
        max_control: usize,
    ) -> Result<Self, ParseError> {
        if !(1..=8).contains(&sample_bytes) {
            return Err(ParseError {
                offset: 0,
                kind: ErrorKind::InvalidSampleBytes,
            });
        }
        Ok(Self {
            sample_bytes,
            max_frame,
            max_control,
            buffer: Vec::new(),
            offset: 0,
            terminal: None,
            control_scan: 0,
        })
    }
    pub fn feed<F: FnMut(Record)>(&mut self, bytes: &[u8], mut cb: F) -> Result<(), ParseError> {
        if let Some(e) = &self.terminal {
            return Err(e.clone());
        }
        for b in bytes {
            self.buffer.push(*b);
            if let Err(e) = self.parse(&mut cb, false) {
                self.terminal = Some(e.clone());
                return Err(e);
            }
        }
        Ok(())
    }
    pub fn finish<F: FnMut(Record)>(&mut self, mut cb: F) -> Result<(), ParseError> {
        if let Some(e) = &self.terminal {
            return Err(e.clone());
        }
        if let Err(e) = self.parse(&mut cb, true) {
            self.terminal = Some(e.clone());
            return Err(e);
        }
        Ok(())
    }
    fn parse<F: FnMut(Record)>(&mut self, cb: &mut F, eof: bool) -> Result<(), ParseError> {
        loop {
            if self.buffer.is_empty() {
                return Ok(());
            }
            if self.buffer[0] == b'<' {
                let nl = self.buffer[self.control_scan..]
                    .iter()
                    .position(|b| *b == b'\n')
                    .map(|n| n + self.control_scan);
                self.control_scan = self.buffer.len();
                if nl.is_none() {
                    if self.buffer.len() > self.max_control {
                        return self.fail(ErrorKind::ControlTooLong);
                    }
                    if eof {
                        return self.fail(ErrorKind::Truncated {
                            needed: self.buffer.len() + 1,
                            available: self.buffer.len(),
                        });
                    }
                    return Ok(());
                }
                let n = nl.unwrap() + 1;
                if n > self.max_control {
                    return self.fail(ErrorKind::ControlTooLong);
                }
                if !valid_control(&self.buffer[..n]) {
                    return self.fail(ErrorKind::InvalidControl);
                }
                let bytes = self.take(n);
                self.control_scan = 0;
                cb(Record::Control {
                    offset: self.offset - n as u64,
                    bytes,
                });
                continue;
            }
            if self.buffer.len() < HEADER_LEN {
                if eof {
                    return self.fail(ErrorKind::Truncated {
                        needed: HEADER_LEN,
                        available: self.buffer.len(),
                    });
                }
                return Ok(());
            }
            let h = Header {
                mask: u32::from_le_bytes(self.buffer[0..4].try_into().unwrap()),
                pcm_len: u32::from_le_bytes(self.buffer[4..8].try_into().unwrap()),
                pos_len: u32::from_le_bytes(self.buffer[8..12].try_into().unwrap()),
                meta_len: u32::from_le_bytes(self.buffer[12..16].try_into().unwrap()),
                pic_len: u32::from_le_bytes(self.buffer[16..20].try_into().unwrap()),
                reserved: self.buffer[20..32].try_into().unwrap(),
            };
            let pcm = (h.pcm_len as usize)
                .checked_mul(self.sample_bytes)
                .ok_or_else(|| self.make_error(ErrorKind::ArithmeticOverflow))?;
            let payload = pcm
                .checked_add(h.pos_len as usize)
                .and_then(|n| n.checked_add(h.meta_len as usize))
                .and_then(|n| n.checked_add(h.pic_len as usize))
                .ok_or_else(|| self.make_error(ErrorKind::ArithmeticOverflow))?;
            let total = HEADER_LEN
                .checked_add(payload)
                .ok_or_else(|| self.make_error(ErrorKind::ArithmeticOverflow))?;
            if total > self.max_frame {
                return self.fail(ErrorKind::FrameTooLarge);
            }
            if self.buffer.len() < total {
                if eof {
                    return self.fail(ErrorKind::Truncated {
                        needed: total,
                        available: self.buffer.len(),
                    });
                }
                return Ok(());
            }
            let start = self.offset;
            self.take(HEADER_LEN);
            let pcm_bytes = self.take(pcm);
            let position = self.take(h.pos_len as usize);
            let metadata = self.take(h.meta_len as usize);
            let picture = self.take(h.pic_len as usize);
            cb(Record::Audio(AudioFrame {
                offset: start,
                header: h,
                pcm: pcm_bytes,
                position,
                metadata,
                picture,
            }))
        }
    }
    fn take(&mut self, n: usize) -> Vec<u8> {
        let x = self.buffer.drain(..n).collect();
        self.offset += n as u64;
        x
    }
    fn make_error(&self, k: ErrorKind) -> ParseError {
        ParseError {
            offset: self.offset,
            kind: k,
        }
    }
    fn fail<T>(&self, k: ErrorKind) -> Result<T, ParseError> {
        Err(self.make_error(k))
    }
}
fn valid_control(line: &[u8]) -> bool {
    let mut body = line;
    if body.starts_with(b"<?xml") {
        let Some(end) = body.windows(2).position(|w| w == b"?>") else {
            return false;
        };
        body = &body[end + 2..];
    }
    [b"<networkaudio".as_slice(), b"<authenticate".as_slice()]
        .iter()
        .any(|root| {
            body.starts_with(root)
                && body.get(root.len()).map_or(false, |c| {
                    *c == b'>' || *c == b' ' || *c == b'/' || *c == b'\t'
                })
        })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn frame() -> Vec<u8> {
        let mut x = Vec::new();
        for n in [7u32, 2, 1, 3, 1] {
            x.extend_from_slice(&n.to_le_bytes())
        }
        x.extend_from_slice(&[0xa5; 12]);
        x.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
        x
    }
    #[test]
    fn s3_prefix_and_poison() {
        let a = b"<networkaudio><operation type=\"keepalive\"/></networkaudio>\n";
        let mut p = Parser::new(4, 1024, 128).unwrap();
        let mut n = 0;
        p.feed(a, |_| n += 1).unwrap();
        assert_eq!(n, 1);
        let e = p.feed(b"<nope>\n", |_| {}).unwrap_err();
        assert_eq!(e.offset, a.len() as u64);
        assert_eq!(p.finish(|_| {}).unwrap_err(), e)
    }
    #[test]
    fn fragmentation_coalescing_and_payload_sections() {
        let control = b"<networkaudio>\n";
        let mut all = control.to_vec();
        let mut payload = frame();
        payload[32] = b'<';
        payload[33] = b'n';
        all.extend(&payload);
        let mut p = Parser::new(4, 1024, 128).unwrap();
        let mut got = Vec::new();
        for b in &all {
            p.feed(std::slice::from_ref(b), |r| got.push(r)).unwrap();
        }
        p.finish(|r| got.push(r)).unwrap();
        assert_eq!(
            got,
            vec![
                Record::Control {
                    offset: 0,
                    bytes: control.to_vec()
                },
                Record::Audio(AudioFrame {
                    offset: control.len() as u64,
                    header: Header {
                        mask: 7,
                        pcm_len: 2,
                        pos_len: 1,
                        meta_len: 3,
                        pic_len: 1,
                        reserved: [0xa5; 12]
                    },
                    pcm: payload[32..40].to_vec(),
                    position: vec![9],
                    metadata: vec![10, 11, 12],
                    picture: vec![13],
                }),
            ]
        );
    }
    #[test]
    fn malformed_truncated_oversized() {
        let mut p = Parser::new(4, 1024, 128).unwrap();
        p.feed(&frame()[..40], |_| {}).unwrap();
        assert!(matches!(
            p.finish(|_| {}),
            Err(ParseError {
                kind: ErrorKind::Truncated { .. },
                ..
            })
        ));
        let mut q = Parser::new(4, 16, 128).unwrap();
        assert!(matches!(
            q.feed(&frame(), |_| {}),
            Err(ParseError {
                kind: ErrorKind::FrameTooLarge,
                ..
            })
        ));
        let mut r = Parser::new(4, 1024, 128).unwrap();
        assert!(matches!(
            r.feed(b"<nope>\n", |_| {}),
            Err(ParseError {
                kind: ErrorKind::InvalidControl,
                ..
            })
        ))
    }
    #[test]
    fn many_frames_callback() {
        let mut p = Parser::new(1, 64, 64).unwrap();
        let mut count = 0;
        let mut x = Vec::new();
        for _ in 0..100 {
            x.extend_from_slice(&[0; 32])
        }
        p.feed(&x, |_| count += 1).unwrap();
        assert_eq!(count, 100)
    }

    #[test]
    fn xml_declaration_authenticate_and_networkaudio_all_splits() {
        let a = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><authenticate public_key=\"synthetic\" signature=\"synthetic\"/>\n";
        let b = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><networkaudio><operation type=\"keepalive\"/></networkaudio>\n";
        let mut input = a.to_vec();
        input.extend_from_slice(b);
        for split in 0..=input.len() {
            let mut p = Parser::new(4, 1024, 1024).unwrap();
            let mut got = Vec::new();
            p.feed(&input[..split], |r| got.push(r)).unwrap();
            p.feed(&input[split..], |r| got.push(r)).unwrap();
            p.finish(|r| got.push(r)).unwrap();
            assert_eq!(
                got,
                vec![
                    Record::Control {
                        offset: 0,
                        bytes: a.to_vec()
                    },
                    Record::Control {
                        offset: a.len() as u64,
                        bytes: b.to_vec()
                    }
                ]
            );
        }
        let mut p = Parser::new(4, 1024, 128).unwrap();
        assert!(matches!(
            p.feed(b"<networkaudiofake>\n", |_| {}),
            Err(ParseError {
                kind: ErrorKind::InvalidControl,
                ..
            })
        ));
    }
}
