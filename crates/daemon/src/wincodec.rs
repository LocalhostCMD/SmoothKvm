//! Picture coding for app windows shown on another computer.
//!
//! The picture is cut into 64x64 tiles. Only tiles that changed since the last frame are sent, as
//! deflate(new XOR old) over the 3 colour bytes of each pixel: unchanged pixels turn into zeros, which compress
//! to almost nothing, so a moving cursor or a blinking caret costs a few hundred bytes, and a still window costs
//! nothing at all. Both sides keep the previous frame, so XOR works from the very first frame (old = black).
//! Packets stay under one encrypted frame (`MAX_PACKET`).
//!
//! Packet layout, repeated: x u16, y u16, w u16, h u16, len u32, then `len` bytes of deflate data (little endian).
// Windows uses the encoder and Linux the decoder (for now), so each build leaves half of this unused.
#![allow(dead_code)]
use std::io;

pub const TILE: usize = 64;
pub const MAX_PACKET: usize = 60_000;
/// Largest picture accepted (8K); anything bigger is refused rather than allocated.
pub const MAX_SIDE: u32 = 8192;

fn bad(m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, m.to_string())
}

pub struct Encoder {
    w: usize,
    h: usize,
    /// previous frame, BGRA, w*h*4
    prev: Vec<u8>,
}

impl Encoder {
    pub fn new(w: u32, h: u32) -> Self {
        let (w, h) = (w.min(MAX_SIDE) as usize, h.min(MAX_SIDE) as usize);
        Self { w, h, prev: vec![0; w * h * 4] }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.w as u32, self.h as u32)
    }

    /// Encode a BGRA frame (top-down rows, `w*4` bytes each). Returns the packets to send (empty = no change).
    pub fn encode(&mut self, frame: &[u8]) -> Vec<Vec<u8>> {
        let (w, h) = (self.w, self.h);
        if frame.len() < w * h * 4 {
            return vec![];
        }
        let mut packets = Vec::new();
        let mut cur: Vec<u8> = Vec::new();
        let mut xor = Vec::with_capacity(TILE * TILE * 3);
        for ty in (0..h).step_by(TILE) {
            let th = TILE.min(h - ty);
            for tx in (0..w).step_by(TILE) {
                let tw = TILE.min(w - tx);
                // quick check: did anything in this tile change?
                let changed = (ty..ty + th).any(|y| {
                    let a = (y * w + tx) * 4;
                    frame[a..a + tw * 4] != self.prev[a..a + tw * 4]
                });
                if !changed {
                    continue;
                }
                xor.clear();
                for y in ty..ty + th {
                    let a = (y * w + tx) * 4;
                    for i in 0..tw {
                        let p = a + i * 4;
                        xor.push(frame[p] ^ self.prev[p]);
                        xor.push(frame[p + 1] ^ self.prev[p + 1]);
                        xor.push(frame[p + 2] ^ self.prev[p + 2]);
                    }
                    self.prev[a..a + tw * 4].copy_from_slice(&frame[a..a + tw * 4]);
                }
                let data = miniz_oxide::deflate::compress_to_vec(&xor, 1);
                if !cur.is_empty() && cur.len() + 12 + data.len() > MAX_PACKET {
                    packets.push(std::mem::take(&mut cur));
                }
                for v in [tx as u16, ty as u16, tw as u16, th as u16] {
                    cur.extend_from_slice(&v.to_le_bytes());
                }
                cur.extend_from_slice(&(data.len() as u32).to_le_bytes());
                cur.extend_from_slice(&data);
            }
        }
        if !cur.is_empty() {
            packets.push(cur);
        }
        packets
    }
}

pub struct Decoder {
    pub w: usize,
    pub h: usize,
    /// current picture, BGRA (alpha always 255)
    pub buf: Vec<u8>,
}

impl Decoder {
    pub fn new(w: u32, h: u32) -> io::Result<Self> {
        if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
            return Err(bad("window size out of range"));
        }
        let (w, h) = (w as usize, h as usize);
        let mut buf = vec![0u8; w * h * 4];
        for px in buf.chunks_exact_mut(4) {
            px[3] = 255;
        }
        Ok(Self { w, h, buf })
    }

    /// Apply one packet; returns the rectangles that changed (x, y, w, h).
    pub fn apply(&mut self, packet: &[u8]) -> io::Result<Vec<(usize, usize, usize, usize)>> {
        let mut rects = Vec::new();
        let mut p = 0usize;
        let rd16 = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]) as usize;
        while p < packet.len() {
            if p + 12 > packet.len() {
                return Err(bad("truncated tile header"));
            }
            let (x, y, tw, th) = (rd16(packet, p), rd16(packet, p + 2), rd16(packet, p + 4), rd16(packet, p + 6));
            let len = u32::from_le_bytes(packet[p + 8..p + 12].try_into().unwrap()) as usize;
            p += 12;
            if tw == 0 || th == 0 || tw > TILE || th > TILE || x + tw > self.w || y + th > self.h {
                return Err(bad("tile outside the window"));
            }
            let data = packet.get(p..p + len).ok_or_else(|| bad("truncated tile"))?;
            p += len;
            let want = tw * th * 3;
            let xor = miniz_oxide::inflate::decompress_to_vec_with_limit(data, want).map_err(|_| bad("bad tile data"))?;
            if xor.len() != want {
                return Err(bad("tile has the wrong size"));
            }
            let mut i = 0;
            for row in y..y + th {
                let a = (row * self.w + x) * 4;
                for c in 0..tw {
                    let q = a + c * 4;
                    self.buf[q] ^= xor[i];
                    self.buf[q + 1] ^= xor[i + 1];
                    self.buf[q + 2] ^= xor[i + 2];
                    i += 3;
                }
            }
            rects.push((x, y, tw, th));
        }
        Ok(rects)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: usize, h: usize, seed: u32) -> Vec<u8> {
        let mut s = seed;
        (0..w * h * 4)
            .map(|i| {
                s = s.wrapping_mul(1103515245).wrapping_add(12345);
                if i % 4 == 3 {
                    255
                } else {
                    (s >> 16) as u8
                }
            })
            .collect()
    }

    fn same_rgb(a: &[u8], b: &[u8]) -> bool {
        a.chunks_exact(4).zip(b.chunks_exact(4)).all(|(x, y)| x[..3] == y[..3])
    }

    #[test]
    fn frames_arrive_exactly_and_only_changes_are_sent() {
        let (w, h) = (200usize, 130usize); // not a multiple of the tile size on purpose
        let mut enc = Encoder::new(w as u32, h as u32);
        let mut dec = Decoder::new(w as u32, h as u32).unwrap();
        let f1 = frame(w, h, 1);
        for p in enc.encode(&f1) {
            assert!(p.len() <= MAX_PACKET);
            dec.apply(&p).unwrap();
        }
        assert!(same_rgb(&dec.buf, &f1));
        // nothing changed: nothing to send
        assert!(enc.encode(&f1).is_empty());
        // a small change (a "cursor") touches one tile and is tiny
        let mut f2 = f1.clone();
        for y in 70..80 {
            for x in 150..160 {
                let i = (y * w + x) * 4;
                f2[i] = 0;
                f2[i + 1] = 0;
                f2[i + 2] = 255;
            }
        }
        let packets = enc.encode(&f2);
        let rects: Vec<_> = packets.iter().flat_map(|p| dec.apply(p).unwrap()).collect();
        assert_eq!(rects, vec![(128, 64, 64, 64)]);
        assert!(packets.iter().map(|p| p.len()).sum::<usize>() < 1500, "a small change stays small");
        assert!(same_rgb(&dec.buf, &f2));
    }

    #[test]
    fn hostile_packets_are_refused() {
        let mut dec = Decoder::new(100, 100).unwrap();
        assert!(dec.apply(&[1, 2, 3]).is_err());
        let mut p = Vec::new();
        for v in [90u16, 90, 64, 64] {
            p.extend_from_slice(&v.to_le_bytes());
        }
        p.extend_from_slice(&0u32.to_le_bytes());
        assert!(dec.apply(&p).is_err(), "tile past the edge");
        assert!(Decoder::new(100_000, 10).is_err());
        // a "zip bomb" tile: decompresses to far more than a tile, must be refused without blowing up
        let bomb = miniz_oxide::deflate::compress_to_vec(&vec![0u8; 10_000_000], 9);
        let mut q = Vec::new();
        for v in [0u16, 0, 64, 64] {
            q.extend_from_slice(&v.to_le_bytes());
        }
        q.extend_from_slice(&(bomb.len() as u32).to_le_bytes());
        q.extend_from_slice(&bomb);
        assert!(dec.apply(&q).is_err());
    }
}
