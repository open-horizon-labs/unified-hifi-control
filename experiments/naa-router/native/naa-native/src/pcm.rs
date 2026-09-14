//! Portable PCM session primitives used by the private Android adapter.
//! This module deliberately does not convert samples: the bytes accepted by
//! `Session::push` are the bytes returned by `Session::pop`.
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmFormat {
    pub rate: u32,
    pub valid_bits: u16,
    pub container_bits: u16,
    pub channels: u16,
    pub endian: Endian,
    pub kind: u16,
    pub dsd_rate: u32,
}
impl PcmFormat {
    pub fn new(
        rate: u32,
        valid_bits: u16,
        container_bits: u16,
        channels: u16,
    ) -> Result<Self, FormatError> {
        Self::new_descriptor(
            rate,
            valid_bits,
            container_bits,
            channels,
            Endian::Little,
            1,
            0,
        )
    }
    pub fn new_with_endian(
        rate: u32,
        valid_bits: u16,
        container_bits: u16,
        channels: u16,
        endian: Endian,
    ) -> Result<Self, FormatError> {
        Self::new_descriptor(rate, valid_bits, container_bits, channels, endian, 1, 0)
    }
    pub fn new_descriptor(
        rate: u32,
        valid_bits: u16,
        container_bits: u16,
        channels: u16,
        endian: Endian,
        kind: u16,
        dsd_rate: u32,
    ) -> Result<Self, FormatError> {
        if rate == 0
            || valid_bits == 0
            || valid_bits > container_bits
            || !matches!(container_bits, 8 | 16 | 24 | 32 | 64)
            || channels == 0
            || !matches!(kind, 1 | 2 | 3)
            || (kind == 2
                && (valid_bits != 24 || !matches!(container_bits, 24 | 32) || dsd_rate != 0))
            || (kind == 3
                && (valid_bits != 32
                    || container_bits != 32
                    || channels != 2
                    || endian != Endian::Big
                    || dsd_rate < 2_822_400
                    || rate.checked_mul(32) != Some(dsd_rate)))
            || (kind == 1 && dsd_rate != 0)
        {
            return Err(FormatError::Invalid);
        }
        let _ = (channels as usize)
            .checked_mul((container_bits / 8) as usize)
            .ok_or(FormatError::Overflow)?;
        Ok(Self {
            rate,
            valid_bits,
            container_bits,
            channels,
            endian,
            kind,
            dsd_rate,
        })
    }
    pub fn bytes_per_frame(self) -> Result<usize, FormatError> {
        (self.channels as usize)
            .checked_mul(self.container_bits as usize / 8)
            .ok_or(FormatError::Overflow)
    }
    pub fn bytes_for_frames(self, frames: u64) -> Result<usize, FormatError> {
        self.bytes_per_frame()?
            .checked_mul(usize::try_from(frames).map_err(|_| FormatError::Overflow)?)
            .ok_or(FormatError::Overflow)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    Invalid,
    Overflow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub format: PcmFormat,
}
impl Offer {
    pub fn exact(format: PcmFormat) -> Self {
        Self { format }
    }
}
pub fn exact_grant(offers: &[Offer], requested: PcmFormat) -> Result<PcmFormat, FormatError> {
    offers
        .iter()
        .find(|o| o.format == requested)
        .map(|o| o.format)
        .ok_or(FormatError::Invalid)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Running,
    Draining,
    Aborted,
    Complete,
}
#[derive(Debug, PartialEq, Eq)]
pub enum SessionError {
    Format(FormatError),
    NotRunning,
    Capacity,
    Misaligned,
    Overflow,
}
/// Queue-only accounting: delivered means dequeued, never device-rendered.
pub struct Session {
    format: PcmFormat,
    capacity: usize,
    queue: VecDeque<Vec<u8>>,
    queued: usize,
    state: State,
    generation: u64,
    accepted: u64,
    delivered: u64,
}
impl Session {
    pub fn start(
        format: PcmFormat,
        capacity_frames: usize,
        generation: u64,
    ) -> Result<Self, SessionError> {
        if capacity_frames == 0 {
            return Err(SessionError::Capacity);
        };
        Ok(Self {
            format,
            capacity: format
                .bytes_for_frames(capacity_frames as u64)
                .map_err(SessionError::Format)?,
            queue: VecDeque::new(),
            queued: 0,
            state: State::Running,
            generation,
            accepted: 0,
            delivered: 0,
        })
    }
    pub fn format(&self) -> PcmFormat {
        self.format
    }
    pub fn state(&self) -> State {
        self.state
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn push(&mut self, bytes: Vec<u8>) -> Result<(), SessionError> {
        if self.state != State::Running {
            return Err(SessionError::NotRunning);
        };
        let bpf = self
            .format
            .bytes_per_frame()
            .map_err(SessionError::Format)?;
        if bytes.is_empty() || bytes.len() % bpf != 0 {
            return Err(SessionError::Misaligned);
        };
        if self
            .queued
            .checked_add(bytes.len())
            .ok_or(SessionError::Overflow)?
            > self.capacity
        {
            return Err(SessionError::Capacity);
        };
        self.accepted += (bytes.len() / bpf) as u64;
        self.queued += bytes.len();
        self.queue.push_back(bytes);
        Ok(())
    }
    pub fn pop(&mut self, max_frames: usize) -> Option<Vec<u8>> {
        if max_frames == 0 {
            return None;
        };
        let bpf = self.format.bytes_per_frame().ok()?;
        let limit = max_frames.checked_mul(bpf)?;
        let mut out = Vec::new();
        while let Some(mut block) = self.queue.pop_front() {
            let room = limit - out.len();
            if block.len() <= room {
                self.queued -= block.len();
                out.extend(block);
            } else {
                let split = room - (room % bpf);
                if split == 0 {
                    self.queue.push_front(block);
                    break;
                }
                let rest = block.split_off(split);
                self.queued -= split;
                out.extend(block);
                self.queue.push_front(rest);
                break;
            }
            if out.len() == limit {
                break;
            }
        }
        if out.is_empty() {
            if self.state == State::Draining {
                self.state = State::Complete;
            }
            None
        } else {
            self.delivered += (out.len() / bpf) as u64;
            if self.state == State::Draining && self.queue.is_empty() {
                self.state = State::Complete;
            }
            Some(out)
        }
    }
    pub fn drain(&mut self) -> Result<(), SessionError> {
        if self.state == State::Running {
            self.state = State::Draining;
            Ok(())
        } else {
            Err(SessionError::NotRunning)
        }
    }
    pub fn abort(&mut self) {
        self.queue.clear();
        self.queued = 0;
        self.state = State::Aborted
    }
    pub fn queued_frames(&self) -> u64 {
        (self.queued / self.format.bytes_per_frame().unwrap_or(1)) as u64
    }
    pub fn accepted_frames(&self) -> u64 {
        self.accepted
    }
    pub fn delivered_frames(&self) -> u64 {
        self.delivered
    }
}

/// Versioned binary IPC payloads. The Java `IpcCodec` envelope carries these
/// records; generation makes stale audio/refusal impossible to confuse with a
/// newly opened output.
pub const IPC_VERSION: u16 = 2;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Start = 1,
    Audio = 2,
    Drain = 3,
    Abort = 4,
    Position = 5,
    Grant = 6,
    Refuse = 7,
    Complete = 8,
}
pub fn encode_command(
    command: Command,
    generation: u64,
    payload: &[u8],
) -> Result<Vec<u8>, SessionError> {
    if payload.len() > 1024 * 1024 {
        return Err(SessionError::Capacity);
    };
    let mut out = Vec::with_capacity(16 + payload.len());
    out.extend_from_slice(&IPC_VERSION.to_le_bytes());
    out.extend_from_slice(&(command as u16).to_le_bytes());
    out.extend_from_slice(&generation.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}
pub fn decode_command(bytes: &[u8]) -> Result<(Command, u64, &[u8]), SessionError> {
    if bytes.len() < 16 {
        return Err(SessionError::Misaligned);
    };
    if u16::from_le_bytes([bytes[0], bytes[1]]) != IPC_VERSION {
        return Err(SessionError::Misaligned);
    };
    let c = match u16::from_le_bytes([bytes[2], bytes[3]]) {
        1 => Command::Start,
        2 => Command::Audio,
        3 => Command::Drain,
        4 => Command::Abort,
        5 => Command::Position,
        6 => Command::Grant,
        7 => Command::Refuse,
        8 => Command::Complete,
        _ => return Err(SessionError::Misaligned),
    };
    let g = u64::from_le_bytes(bytes[4..12].try_into().unwrap());
    let n = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    if n > 1024 * 1024 || bytes.len() != 16 + n {
        return Err(SessionError::Misaligned);
    };
    Ok((c, g, &bytes[16..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arbitrary_rates_widths_and_exact_bytes() {
        for &(r, w, c, ch) in &[
            (44100, 16, 16, 2),
            (96000, 24, 32, 2),
            (123457, 20, 32, 6),
            (192000, 32, 64, 2),
        ] {
            let f = PcmFormat::new(r, w, c, ch).unwrap();
            let n = f.bytes_per_frame().unwrap();
            let mut s = Session::start(f, 4, 9).unwrap();
            let x = (0..n * 2).map(|i| (i as u8) ^ 0x5a).collect::<Vec<_>>();
            s.push(x.clone()).unwrap();
            assert_eq!(s.pop(99).unwrap(), x);
            assert_eq!(s.accepted_frames(), 2);
            assert_eq!(s.delivered_frames(), 2);
        }
    }
    #[test]
    fn exact_grant_and_refusal() {
        let a = PcmFormat::new(12345, 24, 32, 2).unwrap();
        let b = PcmFormat::new(12346, 24, 32, 2).unwrap();
        assert_eq!(exact_grant(&[Offer::exact(a)], a), Ok(a));
        assert_eq!(
            exact_grant(&[Offer::exact(a)], b),
            Err(FormatError::Invalid)
        );
    }
    #[test]
    fn dop_and_native_dsd_share_opaque_queue_geometry() {
        let dop = PcmFormat::new_descriptor(176400, 24, 24, 2, Endian::Little, 2, 0).unwrap();
        let dsd = PcmFormat::new_descriptor(88200, 32, 32, 2, Endian::Big, 3, 2822400).unwrap();
        assert!(PcmFormat::new_descriptor(88200, 32, 32, 2, Endian::Big, 3, 2822401).is_err());
        assert!(PcmFormat::new_descriptor(88200, 32, 32, 1, Endian::Big, 3, 2822400).is_err());
        for f in [dop, dsd] {
            let bytes = (0..f.bytes_per_frame().unwrap() * 3)
                .map(|x| x as u8)
                .collect::<Vec<_>>();
            let mut q = Session::start(f, 4, 12).unwrap();
            q.push(bytes.clone()).unwrap();
            assert_eq!(q.pop(99).unwrap(), bytes);
            assert_eq!(q.accepted_frames(), 3);
        }
    }
    #[test]
    fn bounded_drain_abort_and_generation() {
        let f = PcmFormat::new(12345, 16, 16, 2).unwrap();
        let mut s = Session::start(f, 1, 77).unwrap();
        assert!(s.push(vec![1, 2, 3, 4]).is_ok());
        assert_eq!(s.push(vec![5, 6]), Err(SessionError::Misaligned));
        assert!(s.drain().is_ok());
        assert_eq!(s.push(vec![1, 2, 3, 4]), Err(SessionError::NotRunning));
        assert_eq!(s.pop(1), Some(vec![1, 2, 3, 4]));
        assert_eq!(s.state(), State::Complete);
        s.abort();
        assert_eq!(s.state(), State::Aborted);
    }
    #[test]
    fn versioned_commands_reject_stale_or_truncated() {
        let x = encode_command(Command::Drain, 42, b"ok").unwrap();
        let (c, g, p) = decode_command(&x).unwrap();
        assert_eq!((c, g, p), (Command::Drain, 42, b"ok".as_slice()));
        let mut bad = x.clone();
        bad[0] = 1;
        assert_eq!(decode_command(&bad), Err(SessionError::Misaligned));
        assert_eq!(decode_command(&x[..15]), Err(SessionError::Misaligned));
    }
}
