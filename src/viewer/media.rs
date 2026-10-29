//! RFC 6184 非交织 H264 接收端：按 seq 重组 access unit，marker 收口，缺包超时跳洞并发 PLI。
//! 参照实现同款有界缓冲；这里只做 H264（控制端只报 H264 能力，协商不会出别的）。
use std::time::{Duration, Instant};

pub struct Packet { pub seq: u16, pub timestamp: u32, pub marker: bool, pub payload: Vec<u8> }
pub struct Output { pub units: Vec<Vec<u8>>, pub pli: bool }
impl Default for Output { fn default() -> Self { Self { units: Vec::new(), pli: false } } }

const MAX_AU: usize = 8 * 1024 * 1024;

pub struct Receiver {
    next: Option<u16>,
    pending: Vec<Packet>,
    timestamp: Option<u32>,
    au: Vec<u8>,
    fragment: Option<u8>,
    damaged: bool,
    gap_since: Option<Instant>,
    last_packet: Instant,
}

impl Default for Receiver { fn default() -> Self { Self { next: None, pending: Vec::new(), timestamp: None, au: Vec::new(), fragment: None, damaged: false, gap_since: None, last_packet: Instant::now() } } }

impl Receiver {
    pub fn new() -> Self { Self::default() }

    pub fn push(&mut self, p: Packet) -> Output {
        if p.payload.len() > 65536 { return Output { units: Vec::new(), pli: true }; }
        let next = *self.next.get_or_insert(p.seq);
        if p.seq.wrapping_sub(next) >= 32768 || self.pending.iter().any(|q| q.seq == p.seq) { return Output::default(); }
        self.last_packet = Instant::now();
        self.pending.push(p);
        let mut out = Output::default();
        self.drain(&mut out);
        if self.pending.len() >= 32 { self.skip_gap(&mut out); }
        if self.pending.is_empty() { self.gap_since = None } else { self.gap_since.get_or_insert(Instant::now()); }
        out
    }

    /// 定时器兜底：缺的是 AU 最后一个包时，marker 永远不会来。
    pub fn expire(&mut self) -> Output {
        self.expire_at(Instant::now())
    }
    fn expire_at(&mut self, now: Instant) -> Output {
        let mut out = Output::default();
        if self.gap_since.is_some_and(|since| now.duration_since(since) >= Duration::from_millis(40)) && !self.pending.is_empty() {
            self.skip_gap(&mut out);
            self.gap_since = if self.pending.is_empty() { None } else { Some(now) };
        }
        if !self.au.is_empty() && now.duration_since(self.last_packet) >= Duration::from_millis(200) {
            self.au.clear(); self.fragment = None; self.damaged = true; out.pli = true;
        }
        out
    }

    fn skip_gap(&mut self, out: &mut Output) {
        let next = self.next.unwrap();
        self.next = self.pending.iter().min_by_key(|p| p.seq.wrapping_sub(next)).map(|p| p.seq);
        self.au.clear(); self.fragment = None; self.damaged = true; out.pli = true;
        self.drain(out);
    }

    fn drain(&mut self, out: &mut Output) {
        while let Some(i) = self.pending.iter().position(|p| Some(p.seq) == self.next) {
            let p = self.pending.remove(i);
            self.next = Some(p.seq.wrapping_add(1));
            if self.timestamp != Some(p.timestamp) {
                // 没有 marker 就换了时间戳 = 上一个 AU 残缺。
                if !self.au.is_empty() || self.fragment.is_some() { out.pli = true; }
                self.au.clear(); self.fragment = None; self.damaged = false; self.timestamp = Some(p.timestamp);
            }
            if !self.damaged && self.nal(&p.payload).is_err() { self.damaged = true; self.au.clear(); self.fragment = None; out.pli = true; }
            if p.marker {
                if self.fragment.is_some() { out.pli = true; self.damaged = true; }
                if !self.damaged && !self.au.is_empty() { out.units.push(std::mem::take(&mut self.au)); }
                self.au.clear(); self.fragment = None;
            }
        }
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(), ()> {
        if self.au.len() + bytes.len() > MAX_AU { return Err(()); }
        self.au.extend_from_slice(bytes);
        Ok(())
    }

    /// 单 NAL / STAP-A / FU-A 三种打包都还原成 Annex-B。
    fn nal(&mut self, p: &[u8]) -> Result<(), ()> {
        let h = *p.first().ok_or(())?;
        if h & 0x80 != 0 { return Err(()); }
        match h & 31 {
            1..=23 if self.fragment.is_none() => { self.append(&[0, 0, 0, 1])?; self.append(p) }
            24 if self.fragment.is_none() => {
                let mut rest = &p[1..];
                if rest.is_empty() { return Err(()); }
                while !rest.is_empty() {
                    if rest.len() < 2 { return Err(()); }
                    let n = u16::from_be_bytes([rest[0], rest[1]]) as usize;
                    rest = &rest[2..];
                    if n == 0 || rest.len() < n || rest[0] & 0x80 != 0 || !(1..=23).contains(&(rest[0] & 31)) { return Err(()); }
                    self.append(&[0, 0, 0, 1])?;
                    self.append(&rest[..n])?;
                    rest = &rest[n..];
                }
                Ok(())
            }
            28 if p.len() > 2 => {
                let f = p[1];
                let kind = f & 31;
                let start = f & 128 != 0;
                let end = f & 64 != 0;
                if f & 32 != 0 || kind == 0 || kind > 23 || (start && end) { return Err(()); }
                let header = (h & 0xe0) | kind;
                if start {
                    if self.fragment.is_some() { return Err(()); }
                    self.append(&[0, 0, 0, 1, header])?;
                    self.fragment = Some(header);
                } else if self.fragment != Some(header) { return Err(()); }
                self.append(&p[2..])?;
                if end { self.fragment = None; }
                Ok(())
            }
            _ => Err(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pkt(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> Packet { Packet { seq, timestamp: ts, marker, payload: payload.to_vec() } }
    #[test]
    fn single_nal_marker_yields_unit() {
        let mut r = Receiver::new();
        let out = r.push(pkt(1, 100, true, &[0x65, 1, 2, 3]));
        assert_eq!(out.units, vec![vec![0u8, 0, 0, 1, 0x65, 1, 2, 3]]);
        assert!(!out.pli);
    }
    #[test]
    fn fu_a_reassembles_across_packets() {
        let mut r = Receiver::new();
        assert!(r.push(pkt(1, 100, false, &[0x1c, 0x85, 1, 2])).units.is_empty()); // FU start
        assert!(r.push(pkt(2, 100, false, &[0x1c, 0x05, 3, 4])).units.is_empty()); // FU cont
        let out = r.push(pkt(3, 100, true, &[0x1c, 0x45, 5, 6])); // FU end + marker
        assert_eq!(out.units.len(), 1);
        assert_eq!(out.units[0], vec![0, 0, 0, 1, 0x05, 1, 2, 3, 4, 5, 6]);
        assert!(!out.pli);
    }
    #[test]
    fn missing_packet_expires_to_pli() {
        let mut r = Receiver::new();
        r.push(pkt(1, 100, false, &[0x65, 1]));
        r.push(pkt(3, 100, true, &[0x65, 2]));
        // 缓冲里等不到 2 号包；积压到阈值必须跳洞并发 PLI
        let out = r.expire();
        assert!(!out.pli);
        let mut saw = false;
        for i in 0..32 { if r.push(pkt(4 + i as u16, 200, true, &[0x65, 3])).pli { saw = true; } }
        assert!(saw);
    }
    #[test]
    fn out_of_order_buffers_until_next_seq() {
        let mut r = Receiver::new();
        // 基线从首个包学起（中途入会看不到更早的包）；之后的乱序才是真正的乱序。
        let out = r.push(pkt(0, 100, false, &[0x65, 0]));
        assert!(out.units.is_empty());
        assert!(r.push(pkt(2, 100, true, &[0x65, 2])).units.is_empty());
        let out = r.push(pkt(1, 100, false, &[0x65, 1]));
        assert_eq!(out.units.len(), 1);
        assert_eq!(out.units[0], vec![0, 0, 0, 1, 0x65, 0, 0, 0, 0, 1, 0x65, 1, 0, 0, 0, 1, 0x65, 2]);
    }
}
