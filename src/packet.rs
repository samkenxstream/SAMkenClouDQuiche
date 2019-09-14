// Copyright (C) 2018, Cloudflare, Inc.
// Copyright (C) 2018, Alessandro Ghedini
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
//     * Redistributions of source code must retain the above copyright notice,
//       this list of conditions and the following disclaimer.
//
//     * Redistributions in binary form must reproduce the above copyright
//       notice, this list of conditions and the following disclaimer in the
//       documentation and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS
// IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO,
// THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
// PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
// EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
// PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
// PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
// LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
// NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
// SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

use std::time;

use crate::Error;
use crate::Result;

use crate::crypto;
use crate::octets;
use crate::rand;
use crate::ranges;
use crate::stream;

const FORM_BIT: u8 = 0x80;
const FIXED_BIT: u8 = 0x40;
const KEY_PHASE_BIT: u8 = 0x04;

const TYPE_MASK: u8 = 0x30;
const PKT_NUM_MASK: u8 = 0x03;

pub const MAX_CID_LEN: u8 = 20;

const MAX_PKT_NUM_LEN: usize = 4;
const SAMPLE_LEN: usize = 16;

pub const EPOCH_INITIAL: usize = 0;
pub const EPOCH_HANDSHAKE: usize = 1;
pub const EPOCH_APPLICATION: usize = 2;
pub const EPOCH_COUNT: usize = 3;

/// Packet number space epoch.
///
/// This should only ever be one of `EPOCH_INITIAL`, `EPOCH_HANDSHAKE` or
/// `EPOCH_APPLICATION`, and can be used to index state specific to a packet
/// number space in `Connection` and `Recovery`.
pub type Epoch = usize;

/// QUIC packet type.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Type {
    /// Initial packet.
    Initial,

    /// Retry packet.
    Retry,

    /// Handshake packet.
    Handshake,

    /// 0-RTT packet.
    ZeroRTT,

    /// Version negotiation packet.
    VersionNegotiation,

    /// Short header packet.
    Application,
}

impl Type {
    pub(crate) fn from_epoch(e: Epoch) -> Type {
        match e {
            EPOCH_INITIAL => Type::Initial,

            EPOCH_HANDSHAKE => Type::Handshake,

            EPOCH_APPLICATION => Type::Application,

            _ => unreachable!(),
        }
    }

    pub(crate) fn to_epoch(self) -> Result<Epoch> {
        match self {
            Type::Initial => Ok(EPOCH_INITIAL),

            Type::ZeroRTT => Ok(EPOCH_APPLICATION),

            Type::Handshake => Ok(EPOCH_HANDSHAKE),

            Type::Application => Ok(EPOCH_APPLICATION),

            _ => Err(Error::InvalidPacket),
        }
    }
}

/// A QUIC packet's header.
#[derive(Clone, PartialEq)]
pub struct Header {
    /// The type of the packet.
    pub ty: Type,

    /// The version of the packet.
    pub version: u32,

    /// The destination connection ID of the packet.
    pub dcid: Vec<u8>,

    /// The source connection ID of the packet.
    pub scid: Vec<u8>,

    /// The original destination connection ID. Only present in `Retry`
    /// packets.
    pub odcid: Option<Vec<u8>>,

    /// The packet number. It's only meaningful after the header protection is
    /// removed.
    pub(crate) pkt_num: u64,

    /// The length of the packet number. It's only meaningful after the header
    /// protection is removed.
    pub(crate) pkt_num_len: usize,

    /// The address verification token of the packet. Only present in `Initial`
    /// and `Retry` packets.
    pub token: Option<Vec<u8>>,

    /// The list of versions in the packet. Only present in
    /// `VersionNegotiation` packets.
    pub versions: Option<Vec<u32>>,

    /// The key phase bit of the packet. It's only meaningful after the header
    /// protection is removed.
    pub(crate) key_phase: bool,
}

impl Header {
    /// Parses a QUIC packet header from the given buffer.
    ///
    /// The `dcid_len` parameter is the length of the destination connection ID,
    /// required to parse short header packets.
    ///
    /// ## Examples:
    ///
    /// ```no_run
    /// # const LOCAL_CONN_ID_LEN: usize = 16;
    /// # let mut buf = [0; 512];
    /// # let mut out = [0; 512];
    /// # let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    /// let (len, src) = socket.recv_from(&mut buf).unwrap();
    ///
    /// let hdr = quiche::Header::from_slice(&mut buf[..len], LOCAL_CONN_ID_LEN)?;
    /// # Ok::<(), quiche::Error>(())
    /// ```
    pub fn from_slice(buf: &mut [u8], dcid_len: usize) -> Result<Header> {
        let mut b = octets::Octets::with_slice(buf);
        Header::from_bytes(&mut b, dcid_len)
    }

    pub(crate) fn from_bytes(
        b: &mut octets::Octets, dcid_len: usize,
    ) -> Result<Header> {
        let first = b.get_u8()?;

        if !Header::is_long(first) {
            // Decode short header.
            let dcid = b.get_bytes(dcid_len)?;

            return Ok(Header {
                ty: Type::Application,
                version: 0,
                dcid: dcid.to_vec(),
                scid: Vec::new(),
                odcid: None,
                pkt_num: 0,
                pkt_num_len: 0,
                token: None,
                versions: None,
                key_phase: false,
            });
        }

        // Decode long header.
        let version = b.get_u32()?;

        let ty = if version == 0 {
            Type::VersionNegotiation
        } else {
            match (first & TYPE_MASK) >> 4 {
                0x00 => Type::Initial,
                0x01 => Type::ZeroRTT,
                0x02 => Type::Handshake,
                0x03 => Type::Retry,
                _ => return Err(Error::InvalidPacket),
            }
        };

        let dcid_len = b.get_u8()?;
        if version == crate::PROTOCOL_VERSION && dcid_len > MAX_CID_LEN {
            return Err(Error::InvalidPacket);
        }
        let dcid = b.get_bytes(dcid_len as usize)?.to_vec();

        let scid_len = b.get_u8()?;
        if version == crate::PROTOCOL_VERSION && scid_len > MAX_CID_LEN {
            return Err(Error::InvalidPacket);
        }
        let scid = b.get_bytes(scid_len as usize)?.to_vec();

        // End of invariants.

        let mut odcid: Option<Vec<u8>> = None;
        let mut token: Option<Vec<u8>> = None;
        let mut versions: Option<Vec<u32>> = None;

        match ty {
            Type::Initial => {
                token = Some(b.get_bytes_with_varint_length()?.to_vec());
            },

            Type::Retry => {
                let odcid_len = b.get_u8()?;

                if odcid_len > MAX_CID_LEN {
                    return Err(Error::InvalidPacket);
                }

                odcid = Some(b.get_bytes(odcid_len as usize)?.to_vec());
                token = Some(b.to_vec());
            },

            Type::VersionNegotiation => {
                let mut list: Vec<u32> = Vec::new();

                while b.cap() > 0 {
                    let version = b.get_u32()?;
                    list.push(version);
                }

                versions = Some(list);
            },

            _ => (),
        };

        Ok(Header {
            ty,
            version,
            dcid,
            scid,
            odcid,
            pkt_num: 0,
            pkt_num_len: 0,
            token,
            versions,
            key_phase: false,
        })
    }

    pub(crate) fn to_bytes(&self, out: &mut octets::Octets) -> Result<()> {
        let mut first = 0;

        // Encode pkt num length.
        first |= self.pkt_num_len.saturating_sub(1) as u8;

        // Encode short header.
        if self.ty == Type::Application {
            // Unset form bit for short header.
            first &= !FORM_BIT;

            // Set fixed bit.
            first |= FIXED_BIT;

            // Set key phase bit.
            if self.key_phase {
                first |= KEY_PHASE_BIT;
            } else {
                first &= !KEY_PHASE_BIT;
            }

            out.put_u8(first)?;
            out.put_bytes(&self.dcid)?;

            return Ok(());
        }

        // Encode long header.
        let ty: u8 = match self.ty {
            Type::Initial => 0x00,
            Type::ZeroRTT => 0x01,
            Type::Handshake => 0x02,
            Type::Retry => 0x03,
            _ => return Err(Error::InvalidPacket),
        };

        first |= FORM_BIT | FIXED_BIT | (ty << 4);

        out.put_u8(first)?;

        out.put_u32(self.version)?;

        out.put_u8(self.dcid.len() as u8)?;
        out.put_bytes(&self.dcid)?;

        out.put_u8(self.scid.len() as u8)?;
        out.put_bytes(&self.scid)?;

        if self.ty == Type::Retry {
            let odcid = self.odcid.as_ref().unwrap();
            out.put_u8(odcid.len() as u8)?;
            out.put_bytes(odcid)?;
        }

        // Only Initial and Retry packets have a token.
        if self.ty == Type::Initial {
            match self.token {
                Some(ref v) => {
                    out.put_varint(v.len() as u64)?;
                    out.put_bytes(v)?;
                },

                // No token, so length = 0.
                None => {
                    out.put_varint(0)?;
                },
            }
        }

        // Retry packets don't have a token length.
        if self.ty == Type::Retry {
            out.put_bytes(self.token.as_ref().unwrap())?;
        }

        Ok(())
    }

    /// Returns true if the packet has a long header.
    ///
    /// The `b` parameter represents the first byte of the QUIC header.
    fn is_long(b: u8) -> bool {
        b & FORM_BIT != 0
    }
}

impl std::fmt::Debug for Header {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self.ty)?;

        if self.ty != Type::Application {
            write!(f, " version={:x}", self.version)?;
        }

        write!(f, " dcid=")?;
        for b in &self.dcid {
            write!(f, "{:02x}", b)?;
        }

        if self.ty != Type::Application {
            write!(f, " scid=")?;
            for b in &self.scid {
                write!(f, "{:02x}", b)?;
            }
        }

        if let Some(ref odcid) = self.odcid {
            write!(f, " odcid=")?;
            for b in odcid {
                write!(f, "{:02x}", b)?;
            }
        }

        if let Some(ref token) = self.token {
            write!(f, " token=")?;
            for b in token {
                write!(f, "{:02x}", b)?;
            }
        }

        if let Some(ref versions) = self.versions {
            write!(f, " versions={:x?}", versions)?;
        }

        if self.ty == Type::Application {
            write!(f, " key_phase={}", self.key_phase)?;
        }

        Ok(())
    }
}

pub fn pkt_num_len(pn: u64) -> Result<usize> {
    let len = if pn < u64::from(std::u8::MAX) {
        1
    } else if pn < u64::from(std::u16::MAX) {
        2
    } else if pn < u64::from(std::u32::MAX) {
        4
    } else {
        return Err(Error::InvalidPacket);
    };

    Ok(len)
}

pub fn decrypt_hdr(
    b: &mut octets::Octets, hdr: &mut Header, aead: &crypto::Open,
) -> Result<()> {
    let mut first = {
        let (first_buf, _) = b.split_at(1)?;
        first_buf.as_ref()[0]
    };

    let mut pn_and_sample = b.peek_bytes(MAX_PKT_NUM_LEN + SAMPLE_LEN)?;

    let (mut ciphertext, sample) =
        pn_and_sample.split_at(MAX_PKT_NUM_LEN).unwrap();

    let ciphertext = ciphertext.as_mut();

    let mask = aead.new_mask(sample.as_ref())?;

    if Header::is_long(first) {
        first ^= mask[0] & 0x0f;
    } else {
        first ^= mask[0] & 0x1f;
    }

    let pn_len = usize::from((first & PKT_NUM_MASK) + 1);

    let ciphertext = &mut ciphertext[..pn_len];

    for i in 0..pn_len {
        ciphertext[i] ^= mask[i + 1];
    }

    // Extract packet number corresponding to the decoded length.
    let pn = match pn_len {
        1 => u64::from(b.get_u8()?),

        2 => u64::from(b.get_u16()?),

        3 => u64::from(b.get_u24()?),

        4 => u64::from(b.get_u32()?),

        _ => return Err(Error::InvalidPacket),
    };

    // Write decrypted first byte back into the input buffer.
    let (mut first_buf, _) = b.split_at(1)?;
    first_buf.as_mut()[0] = first;

    hdr.pkt_num = pn;
    hdr.pkt_num_len = pn_len;

    if hdr.ty == Type::Application {
        hdr.key_phase = (first & KEY_PHASE_BIT) != 0;
    }

    Ok(())
}

pub fn decode_pkt_num(largest_pn: u64, truncated_pn: u64, pn_len: usize) -> u64 {
    let pn_nbits = pn_len * 8;
    let expected_pn = largest_pn + 1;
    let pn_win = 1 << pn_nbits;
    let pn_hwin = pn_win / 2;
    let pn_mask = pn_win - 1;
    let candidate_pn = (expected_pn & !pn_mask) | truncated_pn;

    if candidate_pn + pn_hwin <= expected_pn {
        return candidate_pn + pn_win;
    }

    if candidate_pn > expected_pn + pn_hwin && candidate_pn > pn_win {
        return candidate_pn - pn_win;
    }

    candidate_pn
}

pub fn decrypt_pkt<'a>(
    b: &'a mut octets::Octets, pn: u64, pn_len: usize, payload_len: usize,
    aead: &crypto::Open,
) -> Result<octets::Octets<'a>> {
    let payload_offset = b.off();

    let (header, mut payload) = b.split_at(payload_offset)?;

    let mut ciphertext = payload.peek_bytes(payload_len - pn_len)?;

    let payload_len =
        aead.open_with_u64_counter(pn, header.as_ref(), ciphertext.as_mut())?;

    Ok(b.get_bytes(payload_len)?)
}

pub fn encrypt_hdr(
    b: &mut octets::Octets, pn_len: usize, payload: &[u8], aead: &crypto::Seal,
) -> Result<()> {
    let sample = &payload[4 - pn_len..16 + (4 - pn_len)];

    let mask = aead.new_mask(sample)?;

    let (mut first, mut rest) = b.split_at(1)?;

    let first = first.as_mut();

    if Header::is_long(first[0]) {
        first[0] ^= mask[0] & 0x0f;
    } else {
        first[0] ^= mask[0] & 0x1f;
    }

    let pn_buf = rest.slice_last(pn_len)?;
    for i in 0..pn_len {
        pn_buf[i] ^= mask[i + 1];
    }

    Ok(())
}

pub fn encrypt_pkt(
    b: &mut octets::Octets, pn: u64, pn_len: usize, payload_len: usize,
    payload_offset: usize, aead: &crypto::Seal,
) -> Result<usize> {
    let (mut header, mut payload) = b.split_at(payload_offset)?;

    // Encrypt + authenticate payload.
    let ciphertext = payload.slice(payload_len)?;
    aead.seal_with_u64_counter(pn, header.as_ref(), ciphertext)?;

    encrypt_hdr(&mut header, pn_len, ciphertext, aead)?;

    Ok(payload_offset + payload_len)
}

pub fn encode_pkt_num(pn: u64, b: &mut octets::Octets) -> Result<()> {
    let len = pkt_num_len(pn)?;

    match len {
        1 => b.put_u8(pn as u8)?,

        2 => b.put_u16(pn as u16)?,

        3 => b.put_u24(pn as u32)?,

        4 => b.put_u32(pn as u32)?,

        _ => return Err(Error::InvalidPacket),
    };

    Ok(())
}

pub fn negotiate_version(
    scid: &[u8], dcid: &[u8], out: &mut [u8],
) -> Result<usize> {
    let mut b = octets::Octets::with_slice(out);

    let first = rand::rand_u8() | FORM_BIT;

    b.put_u8(first)?;
    b.put_u32(0)?;

    b.put_u8(scid.len() as u8)?;
    b.put_bytes(&scid)?;
    b.put_u8(dcid.len() as u8)?;
    b.put_bytes(&dcid)?;
    b.put_u32(crate::PROTOCOL_VERSION)?;

    Ok(b.off())
}

pub fn retry(
    scid: &[u8], dcid: &[u8], new_scid: &[u8], token: &[u8], out: &mut [u8],
) -> Result<usize> {
    let mut b = octets::Octets::with_slice(out);

    let hdr = Header {
        ty: Type::Retry,
        version: crate::PROTOCOL_VERSION,
        dcid: scid.to_vec(),
        scid: new_scid.to_vec(),
        pkt_num: 0,
        pkt_num_len: 0,
        odcid: Some(dcid.to_vec()),
        token: Some(token.to_vec()),
        versions: None,
        key_phase: false,
    };

    hdr.to_bytes(&mut b)?;

    Ok(b.off())
}

pub struct PktNumSpace {
    pub largest_rx_pkt_num: u64,

    pub largest_rx_pkt_time: time::Instant,

    pub next_pkt_num: u64,

    pub recv_pkt_need_ack: ranges::RangeSet,

    pub recv_pkt_num: PktNumWindow,

    pub ack_elicited: bool,

    pub crypto_open: Option<crypto::Open>,
    pub crypto_seal: Option<crypto::Seal>,

    pub crypto_stream: stream::Stream,
}

impl PktNumSpace {
    pub fn new() -> PktNumSpace {
        PktNumSpace {
            largest_rx_pkt_num: 0,

            largest_rx_pkt_time: time::Instant::now(),

            next_pkt_num: 0,

            recv_pkt_need_ack: ranges::RangeSet::default(),

            recv_pkt_num: PktNumWindow::default(),

            ack_elicited: false,

            crypto_open: None,
            crypto_seal: None,

            crypto_stream: stream::Stream::new(std::u64::MAX, std::u64::MAX),
        }
    }

    pub fn clear(&mut self) {
        self.crypto_stream = stream::Stream::new(std::u64::MAX, std::u64::MAX);

        self.ack_elicited = false;
    }

    pub fn overhead(&self) -> usize {
        self.crypto_seal.as_ref().unwrap().alg().tag_len()
    }

    pub fn ready(&self) -> bool {
        self.crypto_stream.is_flushable() || self.ack_elicited
    }
}

#[derive(Clone, Copy, Default)]
pub struct PktNumWindow {
    lower: u64,
    window: u128,
}

impl PktNumWindow {
    pub fn insert(&mut self, seq: u64) {
        // Packet is on the left end of the window.
        if seq < self.lower {
            return;
        }

        // Packet is on the right end of the window.
        if seq > self.upper() {
            let diff = seq - self.upper();
            self.lower += diff;

            self.window = self.window.checked_shl(diff as u32).unwrap_or(0);
        }

        let mask = 1_u128 << (self.upper() - seq);
        self.window |= mask;
    }

    pub fn contains(&mut self, seq: u64) -> bool {
        // Packet is on the right end of the window.
        if seq > self.upper() {
            return false;
        }

        // Packet is on the left end of the window.
        if seq < self.lower {
            return true;
        }

        let mask = 1_u128 << (self.upper() - seq);
        self.window & mask != 0
    }

    fn upper(&self) -> u64 {
        self.lower
            .saturating_add(std::mem::size_of::<u128>() as u64 * 8) -
            1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::crypto;
    use crate::octets;

    #[test]
    fn retry() {
        let hdr = Header {
            ty: Type::Retry,
            version: 0xafafafaf,
            dcid: vec![0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba],
            scid: vec![0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb],
            pkt_num: 0,
            pkt_num_len: 0,
            odcid: Some(vec![0x01, 0x02, 0x03, 0x04]),
            token: Some(vec![0xba; 24]),
            versions: None,
            key_phase: false,
        };

        let mut d = [0; 52];

        let mut b = octets::Octets::with_slice(&mut d);
        assert!(hdr.to_bytes(&mut b).is_ok());

        let mut b = octets::Octets::with_slice(&mut d);
        assert_eq!(Header::from_bytes(&mut b, 9).unwrap(), hdr);
    }

    #[test]
    fn initial() {
        let hdr = Header {
            ty: Type::Initial,
            version: 0xafafafaf,
            dcid: vec![0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba],
            scid: vec![0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb],
            pkt_num: 0,
            pkt_num_len: 0,
            odcid: None,
            token: Some(vec![0x05, 0x06, 0x07, 0x08]),
            versions: None,
            key_phase: false,
        };

        let mut d = [0; 50];

        let mut b = octets::Octets::with_slice(&mut d);
        assert!(hdr.to_bytes(&mut b).is_ok());

        let mut b = octets::Octets::with_slice(&mut d);
        assert_eq!(Header::from_bytes(&mut b, 9).unwrap(), hdr);
    }

    #[test]
    fn initial_v1_dcid_too_long() {
        let hdr = Header {
            ty: Type::Initial,
            version: crate::PROTOCOL_VERSION,
            dcid: vec![
                0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba,
                0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba,
            ],
            scid: vec![0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb],
            pkt_num: 0,
            pkt_num_len: 0,
            odcid: None,
            token: Some(vec![0x05, 0x06, 0x07, 0x08]),
            versions: None,
            key_phase: false,
        };

        let mut d = [0; 50];

        let mut b = octets::Octets::with_slice(&mut d);
        assert!(hdr.to_bytes(&mut b).is_ok());

        let mut b = octets::Octets::with_slice(&mut d);
        assert_eq!(Header::from_bytes(&mut b, 21), Err(Error::InvalidPacket));
    }

    #[test]
    fn initial_v1_scid_too_long() {
        let hdr = Header {
            ty: Type::Initial,
            version: crate::PROTOCOL_VERSION,
            dcid: vec![0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba],
            scid: vec![
                0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb,
                0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb,
            ],
            pkt_num: 0,
            pkt_num_len: 0,
            odcid: None,
            token: Some(vec![0x05, 0x06, 0x07, 0x08]),
            versions: None,
            key_phase: false,
        };

        let mut d = [0; 50];

        let mut b = octets::Octets::with_slice(&mut d);
        assert!(hdr.to_bytes(&mut b).is_ok());

        let mut b = octets::Octets::with_slice(&mut d);
        assert_eq!(Header::from_bytes(&mut b, 9), Err(Error::InvalidPacket));
    }

    #[test]
    fn initial_non_v1_scid_long() {
        let hdr = Header {
            ty: Type::Initial,
            version: 0xafafafaf,
            dcid: vec![0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba],
            scid: vec![
                0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb,
                0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb,
            ],
            pkt_num: 0,
            pkt_num_len: 0,
            odcid: None,
            token: Some(vec![0x05, 0x06, 0x07, 0x08]),
            versions: None,
            key_phase: false,
        };

        let mut d = [0; 50];

        let mut b = octets::Octets::with_slice(&mut d);
        assert!(hdr.to_bytes(&mut b).is_ok());

        let mut b = octets::Octets::with_slice(&mut d);
        assert_eq!(Header::from_bytes(&mut b, 9).unwrap(), hdr);
    }

    #[test]
    fn handshake() {
        let hdr = Header {
            ty: Type::Handshake,
            version: 0xafafafaf,
            dcid: vec![0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba],
            scid: vec![0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb, 0xbb],
            pkt_num: 0,
            pkt_num_len: 0,
            odcid: None,
            token: None,
            versions: None,
            key_phase: false,
        };

        let mut d = [0; 50];

        let mut b = octets::Octets::with_slice(&mut d);
        assert!(hdr.to_bytes(&mut b).is_ok());

        let mut b = octets::Octets::with_slice(&mut d);
        assert_eq!(Header::from_bytes(&mut b, 9).unwrap(), hdr);
    }

    #[test]
    fn application() {
        let hdr = Header {
            ty: Type::Application,
            version: 0,
            dcid: vec![0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba, 0xba],
            scid: vec![],
            pkt_num: 0,
            pkt_num_len: 0,
            odcid: None,
            token: None,
            versions: None,
            key_phase: false,
        };

        let mut d = [0; 50];

        let mut b = octets::Octets::with_slice(&mut d);
        assert!(hdr.to_bytes(&mut b).is_ok());

        let mut b = octets::Octets::with_slice(&mut d);
        assert_eq!(Header::from_bytes(&mut b, 9).unwrap(), hdr);
    }

    #[test]
    fn pkt_num_decode() {
        let pn = decode_pkt_num(0xa82f30ea, 0x9b32, 2);
        assert_eq!(pn, 0xa82f9b32);
    }

    #[test]
    fn pkt_num_window() {
        let mut win = PktNumWindow::default();
        assert_eq!(win.lower, 0);
        assert!(!win.contains(0));
        assert!(!win.contains(1));

        win.insert(0);
        assert_eq!(win.lower, 0);
        assert!(win.contains(0));
        assert!(!win.contains(1));

        win.insert(1);
        assert_eq!(win.lower, 0);
        assert!(win.contains(0));
        assert!(win.contains(1));

        win.insert(3);
        assert_eq!(win.lower, 0);
        assert!(win.contains(0));
        assert!(win.contains(1));
        assert!(!win.contains(2));
        assert!(win.contains(3));

        win.insert(10);
        assert_eq!(win.lower, 0);
        assert!(win.contains(0));
        assert!(win.contains(1));
        assert!(!win.contains(2));
        assert!(win.contains(3));
        assert!(!win.contains(4));
        assert!(!win.contains(5));
        assert!(!win.contains(6));
        assert!(!win.contains(7));
        assert!(!win.contains(8));
        assert!(!win.contains(9));
        assert!(win.contains(10));

        win.insert(132);
        assert_eq!(win.lower, 5);
        assert!(win.contains(0));
        assert!(win.contains(1));
        assert!(win.contains(2));
        assert!(win.contains(3));
        assert!(win.contains(4));
        assert!(!win.contains(5));
        assert!(!win.contains(6));
        assert!(!win.contains(7));
        assert!(!win.contains(8));
        assert!(!win.contains(9));
        assert!(win.contains(10));
        assert!(!win.contains(128));
        assert!(!win.contains(130));
        assert!(!win.contains(131));
        assert!(win.contains(132));

        win.insert(1024);
        assert_eq!(win.lower, 897);
        assert!(win.contains(0));
        assert!(win.contains(1));
        assert!(win.contains(2));
        assert!(win.contains(3));
        assert!(win.contains(4));
        assert!(win.contains(5));
        assert!(win.contains(6));
        assert!(win.contains(7));
        assert!(win.contains(8));
        assert!(win.contains(9));
        assert!(win.contains(10));
        assert!(win.contains(128));
        assert!(win.contains(130));
        assert!(win.contains(132));
        assert!(win.contains(896));
        assert!(!win.contains(897));
        assert!(!win.contains(1022));
        assert!(!win.contains(1023));
        assert!(win.contains(1024));
        assert!(!win.contains(1025));
        assert!(!win.contains(1026));

        win.insert(std::u64::MAX - 1);
        assert!(win.contains(0));
        assert!(win.contains(1));
        assert!(win.contains(2));
        assert!(win.contains(3));
        assert!(win.contains(4));
        assert!(win.contains(5));
        assert!(win.contains(6));
        assert!(win.contains(7));
        assert!(win.contains(8));
        assert!(win.contains(9));
        assert!(win.contains(10));
        assert!(win.contains(128));
        assert!(win.contains(130));
        assert!(win.contains(132));
        assert!(win.contains(896));
        assert!(win.contains(897));
        assert!(win.contains(1022));
        assert!(win.contains(1023));
        assert!(win.contains(1024));
        assert!(win.contains(1025));
        assert!(win.contains(1026));
        assert!(!win.contains(std::u64::MAX - 2));
        assert!(win.contains(std::u64::MAX - 1));
    }

    fn test_decrypt_pkt(
        pkt: &mut [u8], dcid: &[u8], is_server: bool, expected_frames: &[u8],
        expected_pn: u64, expected_pn_len: usize,
    ) {
        let mut b = octets::Octets::with_slice(pkt);

        let mut hdr = Header::from_bytes(&mut b, 0).unwrap();
        assert_eq!(hdr.ty, Type::Initial);

        let payload_len = b.get_varint().unwrap() as usize;

        let (aead, _) =
            crypto::derive_initial_key_material(dcid, is_server).unwrap();

        decrypt_hdr(&mut b, &mut hdr, &aead).unwrap();
        let pn = decode_pkt_num(0, hdr.pkt_num, hdr.pkt_num_len);

        assert_eq!(hdr.pkt_num, expected_pn);
        assert_eq!(hdr.pkt_num_len, expected_pn_len);

        let payload =
            decrypt_pkt(&mut b, pn, hdr.pkt_num_len, payload_len, &aead).unwrap();

        let payload = payload.as_ref();
        assert_eq!(&payload[..expected_frames.len()], expected_frames);
    }

    #[test]
    fn decrypt_client_initial() {
        let mut pkt = [
            0xc0, 0xff, 0x00, 0x00, 0x17, 0x08, 0x83, 0x94, 0xc8, 0xf0, 0x3e,
            0x51, 0x57, 0x08, 0x00, 0x00, 0x44, 0x9e, 0x3b, 0x34, 0x3a, 0xa8,
            0x53, 0x50, 0x64, 0xa4, 0x26, 0x8a, 0x0d, 0x9d, 0x7b, 0x1c, 0x9d,
            0x25, 0x0a, 0xe3, 0x55, 0x16, 0x22, 0x76, 0xe9, 0xb1, 0xe3, 0x01,
            0x1e, 0xf6, 0xbb, 0xc0, 0xab, 0x48, 0xad, 0x5b, 0xcc, 0x26, 0x81,
            0xe9, 0x53, 0x85, 0x7c, 0xa6, 0x2b, 0xec, 0xd7, 0x52, 0x4d, 0xaa,
            0xc4, 0x73, 0xe6, 0x8d, 0x74, 0x05, 0xfb, 0xba, 0x4e, 0x9e, 0xe6,
            0x16, 0xc8, 0x70, 0x38, 0xbd, 0xbe, 0x90, 0x8c, 0x06, 0xd9, 0x60,
            0x5d, 0x9a, 0xc4, 0x90, 0x30, 0x35, 0x9e, 0xec, 0xb1, 0xd0, 0x5a,
            0x14, 0xe1, 0x17, 0xdb, 0x8c, 0xed, 0xe2, 0xbb, 0x09, 0xd0, 0xdb,
            0xbf, 0xee, 0x27, 0x1c, 0xb3, 0x74, 0xd8, 0xf1, 0x0a, 0xbe, 0xc8,
            0x2d, 0x0f, 0x59, 0xa1, 0xde, 0xe2, 0x9f, 0xe9, 0x56, 0x38, 0xed,
            0x8d, 0xd4, 0x1d, 0xa0, 0x74, 0x87, 0x46, 0x87, 0x91, 0xb7, 0x19,
            0xc5, 0x5c, 0x46, 0x96, 0x8e, 0xb3, 0xb5, 0x46, 0x80, 0x03, 0x71,
            0x02, 0xa2, 0x8e, 0x53, 0xdc, 0x1d, 0x12, 0x90, 0x3d, 0xb0, 0xaf,
            0x58, 0x21, 0x79, 0x4b, 0x41, 0xc4, 0xa9, 0x33, 0x57, 0xfa, 0x59,
            0xce, 0x69, 0xcf, 0xe7, 0xf6, 0xbd, 0xfa, 0x62, 0x9e, 0xef, 0x78,
            0x61, 0x64, 0x47, 0xe1, 0xd6, 0x11, 0xc4, 0xba, 0xf7, 0x1b, 0xf3,
            0x3f, 0xeb, 0xcb, 0x03, 0x13, 0x7c, 0x2c, 0x75, 0xd2, 0x53, 0x17,
            0xd3, 0xe1, 0x3b, 0x68, 0x43, 0x70, 0xf6, 0x68, 0x41, 0x1c, 0x0f,
            0x00, 0x30, 0x4b, 0x50, 0x1c, 0x8f, 0xd4, 0x22, 0xbd, 0x9b, 0x9a,
            0xd8, 0x1d, 0x64, 0x3b, 0x20, 0xda, 0x89, 0xca, 0x05, 0x25, 0xd2,
            0x4d, 0x2b, 0x14, 0x20, 0x41, 0xca, 0xe0, 0xaf, 0x20, 0x50, 0x92,
            0xe4, 0x30, 0x08, 0x0c, 0xd8, 0x55, 0x9e, 0xa4, 0xc5, 0xc6, 0xe4,
            0xfa, 0x3f, 0x66, 0x08, 0x2b, 0x7d, 0x30, 0x3e, 0x52, 0xce, 0x01,
            0x62, 0xba, 0xa9, 0x58, 0x53, 0x2b, 0x0b, 0xbc, 0x2b, 0xc7, 0x85,
            0x68, 0x1f, 0xcf, 0x37, 0x48, 0x5d, 0xff, 0x65, 0x95, 0xe0, 0x1e,
            0x73, 0x9c, 0x8a, 0xc9, 0xef, 0xba, 0x31, 0xb9, 0x85, 0xd5, 0xf6,
            0x56, 0xcc, 0x09, 0x24, 0x32, 0xd7, 0x81, 0xdb, 0x95, 0x22, 0x17,
            0x24, 0x87, 0x64, 0x1c, 0x4d, 0x3a, 0xb8, 0xec, 0xe0, 0x1e, 0x39,
            0xbc, 0x85, 0xb1, 0x54, 0x36, 0x61, 0x47, 0x75, 0xa9, 0x8b, 0xa8,
            0xfa, 0x12, 0xd4, 0x6f, 0x9b, 0x35, 0xe2, 0xa5, 0x5e, 0xb7, 0x2d,
            0x7f, 0x85, 0x18, 0x1a, 0x36, 0x66, 0x63, 0x38, 0x7d, 0xdc, 0x20,
            0x55, 0x18, 0x07, 0xe0, 0x07, 0x67, 0x3b, 0xd7, 0xe2, 0x6b, 0xf9,
            0xb2, 0x9b, 0x5a, 0xb1, 0x0a, 0x1c, 0xa8, 0x7c, 0xbb, 0x7a, 0xd9,
            0x7e, 0x99, 0xeb, 0x66, 0x95, 0x9c, 0x2a, 0x9b, 0xc3, 0xcb, 0xde,
            0x47, 0x07, 0xff, 0x77, 0x20, 0xb1, 0x10, 0xfa, 0x95, 0x35, 0x46,
            0x74, 0xe3, 0x95, 0x81, 0x2e, 0x47, 0xa0, 0xae, 0x53, 0xb4, 0x64,
            0xdc, 0xb2, 0xd1, 0xf3, 0x45, 0xdf, 0x36, 0x0d, 0xc2, 0x27, 0x27,
            0x0c, 0x75, 0x06, 0x76, 0xf6, 0x72, 0x4e, 0xb4, 0x79, 0xf0, 0xd2,
            0xfb, 0xb6, 0x12, 0x44, 0x29, 0x99, 0x04, 0x57, 0xac, 0x6c, 0x91,
            0x67, 0xf4, 0x0a, 0xab, 0x73, 0x99, 0x98, 0xf3, 0x8b, 0x9e, 0xcc,
            0xb2, 0x4f, 0xd4, 0x7c, 0x84, 0x10, 0x13, 0x1b, 0xf6, 0x5a, 0x52,
            0xaf, 0x84, 0x12, 0x75, 0xd5, 0xb3, 0xd1, 0x88, 0x0b, 0x19, 0x7d,
            0xf2, 0xb5, 0xde, 0xa3, 0xe6, 0xde, 0x56, 0xeb, 0xce, 0x3f, 0xfb,
            0x6e, 0x92, 0x77, 0xa8, 0x20, 0x82, 0xf8, 0xd9, 0x67, 0x7a, 0x67,
            0x67, 0x08, 0x9b, 0x67, 0x1e, 0xbd, 0x24, 0x4c, 0x21, 0x4f, 0x0b,
            0xde, 0x95, 0xc2, 0xbe, 0xb0, 0x2c, 0xd1, 0x17, 0x2d, 0x58, 0xbd,
            0xf3, 0x9d, 0xce, 0x56, 0xff, 0x68, 0xeb, 0x35, 0xab, 0x39, 0xb4,
            0x9b, 0x4e, 0xac, 0x7c, 0x81, 0x5e, 0xa6, 0x04, 0x51, 0xd6, 0xe6,
            0xab, 0x82, 0x11, 0x91, 0x18, 0xdf, 0x02, 0xa5, 0x86, 0x84, 0x4a,
            0x9f, 0xfe, 0x16, 0x2b, 0xa0, 0x06, 0xd0, 0x66, 0x9e, 0xf5, 0x76,
            0x68, 0xca, 0xb3, 0x8b, 0x62, 0xf7, 0x1a, 0x25, 0x23, 0xa0, 0x84,
            0x85, 0x2c, 0xd1, 0xd0, 0x79, 0xb3, 0x65, 0x8d, 0xc2, 0xf3, 0xe8,
            0x79, 0x49, 0xb5, 0x50, 0xba, 0xb3, 0xe1, 0x77, 0xcf, 0xc4, 0x9e,
            0xd1, 0x90, 0xdf, 0xf0, 0x63, 0x0e, 0x43, 0x07, 0x7c, 0x30, 0xde,
            0x8f, 0x6a, 0xe0, 0x81, 0x53, 0x7f, 0x1e, 0x83, 0xda, 0x53, 0x7d,
            0xa9, 0x80, 0xaf, 0xa6, 0x68, 0xe7, 0xb7, 0xfb, 0x25, 0x30, 0x1c,
            0xf7, 0x41, 0x52, 0x4b, 0xe3, 0xc4, 0x98, 0x84, 0xb4, 0x28, 0x21,
            0xf1, 0x75, 0x52, 0xfb, 0xd1, 0x93, 0x1a, 0x81, 0x30, 0x17, 0xb6,
            0xb6, 0x59, 0x0a, 0x41, 0xea, 0x18, 0xb6, 0xba, 0x49, 0xcd, 0x48,
            0xa4, 0x40, 0xbd, 0x9a, 0x33, 0x46, 0xa7, 0x62, 0x3f, 0xb4, 0xba,
            0x34, 0xa3, 0xee, 0x57, 0x1e, 0x3c, 0x73, 0x1f, 0x35, 0xa7, 0xa3,
            0xcf, 0x25, 0xb5, 0x51, 0xa6, 0x80, 0xfa, 0x68, 0x76, 0x35, 0x07,
            0xb7, 0xfd, 0xe3, 0xaa, 0xf0, 0x23, 0xc5, 0x0b, 0x9d, 0x22, 0xda,
            0x68, 0x76, 0xba, 0x33, 0x7e, 0xb5, 0xe9, 0xdd, 0x9e, 0xc3, 0xda,
            0xf9, 0x70, 0x24, 0x2b, 0x6c, 0x5a, 0xab, 0x3a, 0xa4, 0xb2, 0x96,
            0xad, 0x8b, 0x9f, 0x68, 0x32, 0xf6, 0x86, 0xef, 0x70, 0xfa, 0x93,
            0x8b, 0x31, 0xb4, 0xe5, 0xdd, 0xd7, 0x36, 0x44, 0x42, 0xd3, 0xea,
            0x72, 0xe7, 0x3d, 0x66, 0x8f, 0xb0, 0x93, 0x77, 0x96, 0xf4, 0x62,
            0x92, 0x3a, 0x81, 0xa4, 0x7e, 0x1c, 0xee, 0x74, 0x26, 0xff, 0x6d,
            0x92, 0x21, 0x26, 0x9b, 0x5a, 0x62, 0xec, 0x03, 0xd6, 0xec, 0x94,
            0xd1, 0x26, 0x06, 0xcb, 0x48, 0x55, 0x60, 0xba, 0xb5, 0x74, 0x81,
            0x60, 0x09, 0xe9, 0x65, 0x04, 0x24, 0x93, 0x85, 0xbb, 0x61, 0xa8,
            0x19, 0xbe, 0x04, 0xf6, 0x2c, 0x20, 0x66, 0x21, 0x4d, 0x83, 0x60,
            0xa2, 0x02, 0x2b, 0xeb, 0x31, 0x62, 0x40, 0xb6, 0xc7, 0xd7, 0x8b,
            0xbe, 0x56, 0xc1, 0x30, 0x82, 0xe0, 0xca, 0x27, 0x26, 0x61, 0x21,
            0x0a, 0xbf, 0x02, 0x0b, 0xf3, 0xb5, 0x78, 0x3f, 0x14, 0x26, 0x43,
            0x6c, 0xf9, 0xff, 0x41, 0x84, 0x05, 0x93, 0xa5, 0xd0, 0x63, 0x8d,
            0x32, 0xfc, 0x51, 0xc5, 0xc6, 0x5f, 0xf2, 0x91, 0xa3, 0xa7, 0xa5,
            0x2f, 0xd6, 0x77, 0x5e, 0x62, 0x3a, 0x44, 0x39, 0xcc, 0x08, 0xdd,
            0x25, 0x58, 0x2f, 0xeb, 0xc9, 0x44, 0xef, 0x92, 0xd8, 0xdb, 0xd3,
            0x29, 0xc9, 0x1d, 0xe3, 0xe9, 0xc9, 0x58, 0x2e, 0x41, 0xf1, 0x7f,
            0x3d, 0x18, 0x6f, 0x10, 0x4a, 0xd3, 0xf9, 0x09, 0x95, 0x11, 0x6c,
            0x68, 0x2a, 0x2a, 0x14, 0xa3, 0xb4, 0xb1, 0xf5, 0x47, 0xc3, 0x35,
            0xf0, 0xbe, 0x71, 0x0f, 0xc9, 0xfc, 0x03, 0xe0, 0xe5, 0x87, 0xb8,
            0xcd, 0xa3, 0x1c, 0xe6, 0x5b, 0x96, 0x98, 0x78, 0xa4, 0xad, 0x42,
            0x83, 0xe6, 0xd5, 0xb0, 0x37, 0x3f, 0x43, 0xda, 0x86, 0xe9, 0xe0,
            0xff, 0xe1, 0xae, 0x0f, 0xdd, 0xd3, 0x51, 0x62, 0x55, 0xbd, 0x74,
            0x56, 0x6f, 0x36, 0xa3, 0x87, 0x03, 0xd5, 0xf3, 0x42, 0x49, 0xde,
            0xd1, 0xf6, 0x6b, 0x3d, 0x9b, 0x45, 0xb9, 0xaf, 0x2c, 0xcf, 0xef,
            0xe9, 0x84, 0xe1, 0x33, 0x76, 0xb1, 0xb2, 0xc6, 0x40, 0x4a, 0xa4,
            0x8c, 0x80, 0x26, 0x13, 0x23, 0x43, 0xda, 0x3f, 0x3a, 0x33, 0x65,
            0x9e, 0xc1, 0xb3, 0xe9, 0x50, 0x80, 0x54, 0x0b, 0x28, 0xb7, 0xf3,
            0xfc, 0xd3, 0x5f, 0xa5, 0xd8, 0x43, 0xb5, 0x79, 0xa8, 0x4c, 0x08,
            0x91, 0x21, 0xa6, 0x0d, 0x8c, 0x17, 0x54, 0x91, 0x5c, 0x34, 0x4e,
            0xea, 0xf4, 0x5a, 0x9b, 0xf2, 0x7d, 0xc0, 0xc1, 0xe7, 0x84, 0x16,
            0x16, 0x91, 0x22, 0x09, 0x13, 0x13, 0xeb, 0x0e, 0x87, 0x55, 0x5a,
            0xbd, 0x70, 0x66, 0x26, 0xe5, 0x57, 0xfc, 0x36, 0xa0, 0x4f, 0xcd,
            0x19, 0x1a, 0x58, 0x82, 0x91, 0x04, 0xd6, 0x07, 0x5c, 0x55, 0x94,
            0xf6, 0x27, 0xca, 0x50, 0x6b, 0xf1, 0x81, 0xda, 0xec, 0x94, 0x0f,
            0x4a, 0x4f, 0x3a, 0xf0, 0x07, 0x4e, 0xee, 0x89, 0xda, 0xac, 0xde,
            0x67, 0x58, 0x31, 0x26, 0x22, 0xd4, 0xfa, 0x67, 0x5b, 0x39, 0xf7,
            0x28, 0xe0, 0x62, 0xd2, 0xbe, 0xe6, 0x80, 0xd8, 0xf4, 0x1a, 0x59,
            0x7c, 0x26, 0x26, 0x48, 0xbb, 0x18, 0xbc, 0xfc, 0x13, 0xc8, 0xb3,
            0xd9, 0x7b, 0x1a, 0x77, 0xb2, 0xac, 0x3a, 0xf7, 0x45, 0xd6, 0x1a,
            0x34, 0xcc, 0x47, 0x09, 0x86, 0x5b, 0xac, 0x82, 0x4a, 0x94, 0xbb,
            0x19, 0x05, 0x80, 0x15, 0xe4, 0xe4, 0x2d, 0xc9, 0xbe, 0x6c, 0x78,
            0x03, 0x56, 0x73, 0x21, 0x82, 0x9d, 0xd8, 0x58, 0x53, 0x39, 0x62,
            0x69,
        ];

        let dcid = [0x83, 0x94, 0xc8, 0xf0, 0x3e, 0x51, 0x57, 0x08];

        let frames = [
            0x06, 0x00, 0x40, 0xc4, 0x01, 0x00, 0x00, 0xc0, 0x03, 0x03, 0x66,
            0x60, 0x26, 0x1f, 0xf9, 0x47, 0xce, 0xa4, 0x9c, 0xce, 0x6c, 0xfa,
            0xd6, 0x87, 0xf4, 0x57, 0xcf, 0x1b, 0x14, 0x53, 0x1b, 0xa1, 0x41,
            0x31, 0xa0, 0xe8, 0xf3, 0x09, 0xa1, 0xd0, 0xb9, 0xc4, 0x00, 0x00,
            0x06, 0x13, 0x01, 0x13, 0x03, 0x13, 0x02, 0x01, 0x00, 0x00, 0x91,
            0x00, 0x00, 0x00, 0x0b, 0x00, 0x09, 0x00, 0x00, 0x06, 0x73, 0x65,
            0x72, 0x76, 0x65, 0x72, 0xff, 0x01, 0x00, 0x01, 0x00, 0x00, 0x0a,
            0x00, 0x14, 0x00, 0x12, 0x00, 0x1d, 0x00, 0x17, 0x00, 0x18, 0x00,
            0x19, 0x01, 0x00, 0x01, 0x01, 0x01, 0x02, 0x01, 0x03, 0x01, 0x04,
            0x00, 0x23, 0x00, 0x00, 0x00, 0x33, 0x00, 0x26, 0x00, 0x24, 0x00,
            0x1d, 0x00, 0x20, 0x4c, 0xfd, 0xfc, 0xd1, 0x78, 0xb7, 0x84, 0xbf,
            0x32, 0x8c, 0xae, 0x79, 0x3b, 0x13, 0x6f, 0x2a, 0xed, 0xce, 0x00,
            0x5f, 0xf1, 0x83, 0xd7, 0xbb, 0x14, 0x95, 0x20, 0x72, 0x36, 0x64,
            0x70, 0x37, 0x00, 0x2b, 0x00, 0x03, 0x02, 0x03, 0x04, 0x00, 0x0d,
            0x00, 0x20, 0x00, 0x1e, 0x04, 0x03, 0x05, 0x03, 0x06, 0x03, 0x02,
            0x03, 0x08, 0x04, 0x08, 0x05, 0x08, 0x06, 0x04, 0x01, 0x05, 0x01,
            0x06, 0x01, 0x02, 0x01, 0x04, 0x02, 0x05, 0x02, 0x06, 0x02, 0x02,
            0x02, 0x00, 0x2d, 0x00, 0x02, 0x01, 0x01, 0x00, 0x1c, 0x00, 0x02,
            0x40, 0x01,
        ];

        test_decrypt_pkt(&mut pkt, &dcid, true, &frames, 2, 4);
    }

    #[test]
    fn decrypt_server_initial() {
        let mut pkt = [
            0xc9, 0xff, 0x00, 0x00, 0x17, 0x00, 0x08, 0xf0, 0x67, 0xa5, 0x50,
            0x2a, 0x42, 0x62, 0xb5, 0x00, 0x40, 0x74, 0x16, 0x8b, 0xf2, 0x2b,
            0x70, 0x02, 0x59, 0x6f, 0x99, 0xae, 0x67, 0xab, 0xf6, 0x5a, 0x58,
            0x52, 0xf5, 0x4f, 0x58, 0xc3, 0x7c, 0x80, 0x86, 0x82, 0xe2, 0xe4,
            0x04, 0x92, 0xd8, 0xa3, 0x89, 0x9f, 0xb0, 0x4f, 0xc0, 0xaf, 0xe9,
            0xaa, 0xbc, 0x87, 0x67, 0xb1, 0x8a, 0x0a, 0xa4, 0x93, 0x53, 0x74,
            0x26, 0x37, 0x3b, 0x48, 0xd5, 0x02, 0x21, 0x4d, 0xd8, 0x56, 0xd6,
            0x3b, 0x78, 0xce, 0xe3, 0x7b, 0xc6, 0x64, 0xb3, 0xfe, 0x86, 0xd4,
            0x87, 0xac, 0x7a, 0x77, 0xc5, 0x30, 0x38, 0xa3, 0xcd, 0x32, 0xf0,
            0xb5, 0x00, 0x4d, 0x9f, 0x57, 0x54, 0xc4, 0xf7, 0xf2, 0xd1, 0xf3,
            0x5c, 0xf3, 0xf7, 0x11, 0x63, 0x51, 0xc9, 0x2b, 0x9c, 0xf9, 0xbb,
            0x6d, 0x09, 0x1d, 0xdf, 0xc8, 0xb3, 0x2d, 0x43, 0x23, 0x48, 0xa2,
            0xc4, 0x13,
        ];

        let dcid = [0x83, 0x94, 0xc8, 0xf0, 0x3e, 0x51, 0x57, 0x08];

        let frames = [
            0x0d, 0x00, 0x00, 0x00, 0x00, 0x18, 0x41, 0x0a, 0x02, 0x00, 0x00,
            0x56, 0x03, 0x03, 0xee, 0xfc, 0xe7, 0xf7, 0xb3, 0x7b, 0xa1, 0xd1,
            0x63, 0x2e, 0x96, 0x67, 0x78, 0x25, 0xdd, 0xf7, 0x39, 0x88, 0xcf,
            0xc7, 0x98, 0x25, 0xdf, 0x56, 0x6d, 0xc5, 0x43, 0x0b, 0x9a, 0x04,
            0x5a, 0x12, 0x00, 0x13, 0x01, 0x00, 0x00, 0x2e, 0x00, 0x33, 0x00,
            0x24, 0x00, 0x1d, 0x00, 0x20, 0x9d, 0x3c, 0x94, 0x0d, 0x89, 0x69,
            0x0b, 0x84, 0xd0, 0x8a, 0x60, 0x99, 0x3c, 0x14, 0x4e, 0xca, 0x68,
            0x4d, 0x10, 0x81, 0x28, 0x7c, 0x83, 0x4d, 0x53, 0x11, 0xbc, 0xf3,
            0x2b, 0xb9, 0xda, 0x1a, 0x00, 0x2b, 0x00, 0x02, 0x03, 0x04,
        ];

        test_decrypt_pkt(&mut pkt, &dcid, false, &frames, 1, 2);
    }
}
