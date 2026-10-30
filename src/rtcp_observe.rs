//! RTCP 观测拦截器：把双端 RTCP 实际收发内容记进日志文件。
//!
//! 为什么要看到 RTCP：客户端"延迟"这一行的正源是它自己的 RTT 估计，而 RTT 只能来自
//! RTCP（SR 的 request-fdb 探测回复，或对端 RR 里的 LSR/DLSR）。没有 RTCP 级日志，
//! 就没法判断"恒 50 ms"是它对探测没回复、还是压根没探测（libwebrtc 收流侧 RTT 默认值
//! 就是 50 ms，没样本时永远显示这个常量）。
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use webrtc::interceptor::{
    stream_info::StreamInfo, Attributes, Error, Interceptor, InterceptorBuilder, RTCPReader,
    RTCPWriter, RTPReader, RTPWriter,
};
use webrtc::rtcp::packet::Packet;
use webrtc::rtcp::receiver_report::ReceiverReport;
use webrtc::rtcp::sender_report::SenderReport;

/// 攒 5 秒记一行，避免 RTCP 每秒几十包把日志刷爆。
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

struct Observe {
    /// (方向, PT, count) → 包数。方向：0=收 1=发。
    kinds: HashMap<(u8, u8, u8), u32>,
    errors: u32,
    /// 收到的 RR 里对端回的 LSR/DLSR（RTT 原料），只留最近一条。
    last_rr: Option<(u32, u32, u32)>,
    /// 我们发出的 SR 快照（回复探测时要用的 ssrc/ntp/rtp 计数）。
    last_sr: Option<(u32, u64, u32, u32, u32)>,
    since: Instant,
}

pub struct RtcpObserveBuilder;

impl InterceptorBuilder for RtcpObserveBuilder {
    fn build(&self, _id: &str) -> Result<Arc<dyn Interceptor + Send + Sync>, Error> {
        Ok(Arc::new(RtcpObserve {
            state: Arc::new(Mutex::new(Observe {
                kinds: HashMap::new(),
                errors: 0,
                last_rr: None,
                last_sr: None,
                since: Instant::now(),
            })),
        }))
    }
}

pub struct RtcpObserve {
    state: Arc<Mutex<Observe>>,
}

#[async_trait::async_trait]
impl Interceptor for RtcpObserve {
    async fn bind_rtcp_reader(
        &self,
        reader: Arc<dyn RTCPReader + Send + Sync>,
    ) -> Arc<dyn RTCPReader + Send + Sync> {
        Arc::new(ObserveReader {
            state: self.state.clone(),
            parent: reader,
        })
    }

    async fn bind_rtcp_writer(
        &self,
        writer: Arc<dyn RTCPWriter + Send + Sync>,
    ) -> Arc<dyn RTCPWriter + Send + Sync> {
        Arc::new(ObserveWriter {
            state: self.state.clone(),
            parent: writer,
        })
    }

    async fn bind_local_stream(
        &self,
        _info: &StreamInfo,
        writer: Arc<dyn RTPWriter + Send + Sync>,
    ) -> Arc<dyn RTPWriter + Send + Sync> {
        writer
    }

    async fn unbind_local_stream(&self, _info: &StreamInfo) {}

    async fn bind_remote_stream(
        &self,
        _info: &StreamInfo,
        reader: Arc<dyn RTPReader + Send + Sync>,
    ) -> Arc<dyn RTPReader + Send + Sync> {
        reader
    }

    async fn unbind_remote_stream(&self, _info: &StreamInfo) {}

    async fn close(&self) -> Result<(), Error> {
        Ok(())
    }
}

fn kind_name(pt: u8, count: u8) -> String {
    match (pt, count) {
        (200, c) => format!("SR(c={c})"),
        (201, 0) => "RR".into(),
        (201, 1) => "RR(1块)".into(),
        (201, 2) => "SR-REQ?".into(),
        (201, c) => format!("PT201(c={c})"),
        (202, _) => "SDES".into(),
        (203, _) => "BYE".into(),
        (204, c) => format!("NACK(c={c})"),
        (205, 15) => "TWCC".into(),
        (205, c) => format!("PT205(c={c})"),
        (206, c) => format!("PT206(c={c})"),
        (207, _) => "XR".into(),
        (p, c) => format!("PT{p}(c={c})"),
    }
}

async fn tally(state: &Arc<Mutex<Observe>>, dir: u8, pkts: &[Box<dyn Packet + Send + Sync>]) {
    let mut s = state.lock().await;
    for p in pkts {
        let h = p.header();
        *s.kinds.entry((dir, h.packet_type as u8, h.count)).or_insert(0) += 1;
        if dir == 0 {
            if let Some(rr) = p.as_any().downcast_ref::<ReceiverReport>() {
                if let Some(b) = rr.reports.first() {
                    s.last_rr = Some((b.ssrc, b.last_sender_report, b.delay));
                }
            }
        } else if let Some(sr) = p.as_any().downcast_ref::<SenderReport>() {
            s.last_sr = Some((sr.ssrc, sr.ntp_time, sr.rtp_time, sr.packet_count, sr.octet_count));
        }
    }
    if s.since.elapsed() >= FLUSH_INTERVAL {
        let mut lines: Vec<(u8, u8, u8, u32)> =
            s.kinds.drain().map(|(k, v)| (k.0, k.1, k.2, v)).collect();
        lines.sort_by_key(|&(d, _, _, v)| (d, std::cmp::Reverse(v)));
        let mut parts = Vec::new();
        for (dir, pt, c, n) in lines {
            parts.push(format!("{} {n}×{}", if dir == 0 { "收" } else { "发" }, kind_name(pt, c)));
        }
        let rr = s
            .last_rr
            .map(|(ssrc, lsr, dl)| format!(" 对端RR→ssrc={ssrc:08x} LSR={lsr:08x} DLSR={dl}"))
            .unwrap_or_default();
        let sr = s
            .last_sr
            .map(|(ssrc, ntp, _, _, _)| format!(" 我方SR ssrc={ssrc:08x} ntp={ntp:016x}"))
            .unwrap_or_default();
        let errors = s.errors;
        s.errors = 0;
        s.since = Instant::now();
        drop(s);
        crate::hostlog::log(&format!(
            "RTCP {}{}{}",
            parts.join(" "),
            rr,
            sr
        ));
        if errors > 0 {
            crate::hostlog::log(&format!("RTCP 解析失败 {errors} 包（含未知类型时整段丢弃）"));
        }
    }
}

struct ObserveReader {
    state: Arc<Mutex<Observe>>,
    parent: Arc<dyn RTCPReader + Send + Sync>,
}

#[async_trait::async_trait]
impl RTCPReader for ObserveReader {
    async fn read(
        &self,
        buf: &mut [u8],
        a: &Attributes,
    ) -> Result<(Vec<Box<dyn Packet + Send + Sync>>, Attributes), Error> {
        match self.parent.read(buf, a).await {
            Ok((pkts, attr)) => {
                tally(&self.state, 0, &pkts).await;
                Ok((pkts, attr))
            }
            Err(e) => {
                self.state.lock().await.errors += 1;
                Err(e)
            }
        }
    }
}

struct ObserveWriter {
    state: Arc<Mutex<Observe>>,
    parent: Arc<dyn RTCPWriter + Send + Sync>,
}

#[async_trait::async_trait]
impl RTCPWriter for ObserveWriter {
    async fn write(
        &self,
        pkts: &[Box<dyn Packet + Send + Sync>],
        a: &Attributes,
    ) -> Result<usize, Error> {
        tally(&self.state, 1, pkts).await;
        self.parent.write(pkts, a).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_names_are_distinguishable() {
        assert_eq!(kind_name(200, 0), "SR(c=0)");
        assert_eq!(kind_name(201, 1), "RR(1块)");
        assert_eq!(kind_name(201, 2), "SR-REQ?");
        assert_eq!(kind_name(205, 15), "TWCC");
        assert_eq!(kind_name(207, 0), "XR");
    }
}
