//! 键盘长按重复。uinput 注入的键内核**不会**自动重复（2026-10-30 实测：声明 EV_REP
//! 的设备按住 3 s，value=2 自动重复事件 = 0 个），而官方客户端用 `kbd_heartbeat`
//! 表达"还按着"。所以长按必须由宿主自己按系统键盘节奏（500 ms 后每 33 ms）重发按下。
//! 纯逻辑、时间从外部注入，可离线单测。

use std::time::{Duration, Instant};

pub struct KeyRepeater {
    delay: Duration,
    period: Duration,
    /// next = 下一次该重发的时刻（press 时 = 按下 + delay）。
    held: Vec<HeldKey>,
}

#[derive(Clone, Copy)]
struct HeldKey {
    code: i32,
    next: Instant,
}

impl KeyRepeater {
    pub fn new(delay: Duration, period: Duration) -> Self {
        Self { delay, period, held: Vec::new() }
    }

    /// 新按下：同码先清账（长按中再收到 press = 客户端重发，重新计时）。
    pub fn press(&mut self, code: i32, at: Instant) {
        self.held.retain(|h| h.code != code);
        self.held.push(HeldKey { code, next: at + self.delay });
    }

    pub fn release(&mut self, code: i32) {
        self.held.retain(|h| h.code != code);
    }

    /// 此刻该重发的键；每个按住键按自己的节奏推进，一次调用最多各发一次
    /// （调用比 period 慢时不补欠账——长按注入本来就不需要回放）。
    pub fn due(&mut self, now: Instant) -> Vec<i32> {
        let mut out = Vec::new();
        for h in self.held.iter_mut() {
            if h.next <= now {
                out.push(h.code);
                h.next += self.period;
                if h.next <= now {
                    h.next = now + self.period;
                }
            }
        }
        out
    }

    /// 当前按住的键码（心跳注入用，不动节奏）。
    pub fn held_codes(&self) -> Vec<i32> {
        self.held.iter().map(|h| h.code).collect()
    }

    /// 会话结束：清空账本。
    pub fn clear(&mut self) {
        self.held.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 键码就是客户端给的 Windows VK（宿主自己记账，注入时再换算）。
    const VK_BACK: i32 = 0x08;
    const VK_DELETE: i32 = 0x2E;

    fn base() -> Instant {
        Instant::now()
    }

    #[test]
    fn repeat_starts_after_delay_then_every_period() {
        let t = base();
        let mut r = KeyRepeater::new(Duration::from_millis(500), Duration::from_millis(33));
        r.press(VK_BACK, t); // Backspace
        assert!(r.due(t + Duration::from_millis(499)).is_empty());
        assert_eq!(r.due(t + Duration::from_millis(500)), vec![VK_BACK]);
        assert!(r.due(t + Duration::from_millis(532)).is_empty());
        assert_eq!(r.due(t + Duration::from_millis(533)), vec![VK_BACK]);
        assert_eq!(r.due(t + Duration::from_millis(566)), vec![VK_BACK]);
    }

    #[test]
    fn release_stops_repeat_and_repress_restarts_delay() {
        let t = base();
        let mut r = KeyRepeater::new(Duration::from_millis(500), Duration::from_millis(33));
        r.press(VK_BACK, t);
        assert_eq!(r.due(t + Duration::from_millis(600)), vec![VK_BACK]);
        r.release(VK_BACK);
        assert!(r.due(t + Duration::from_millis(700)).is_empty());
        r.press(VK_BACK, t + Duration::from_millis(800));
        assert!(r.due(t + Duration::from_millis(1_299)).is_empty());
        assert_eq!(r.due(t + Duration::from_millis(1_300)), vec![VK_BACK]);
    }

    #[test]
    fn two_held_keys_repeat_independently() {
        let t = base();
        let mut r = KeyRepeater::new(Duration::from_millis(500), Duration::from_millis(33));
        r.press(VK_BACK, t);
        r.press(VK_DELETE, t + Duration::from_millis(200));
        // 账本按按下顺序排：t+500 只有 BACK 到点（DELETE 要到 t+700 才开始）。
        assert_eq!(r.due(t + Duration::from_millis(500)), vec![VK_BACK]);
        // t+700 两个都到点（BACK 每 33ms 一路走到 700；DELETE 到点第一次）——
        // 各自节奏独立推进，互不影响。
        assert_eq!(r.due(t + Duration::from_millis(700)), vec![VK_BACK, VK_DELETE]);
        r.release(VK_BACK);
        assert_eq!(r.due(t + Duration::from_millis(733)), vec![VK_DELETE]);
    }

    #[test]
    fn held_codes_lists_held_keys_without_changing_schedule() {
        let t = base();
        let mut r = KeyRepeater::new(Duration::from_millis(500), Duration::from_millis(33));
        r.press(VK_BACK, t);
        assert_eq!(r.held_codes(), vec![VK_BACK]);
        assert!(r.due(t + Duration::from_millis(499)).is_empty()); // 节奏没被打乱
        r.clear();
        assert!(r.held_codes().is_empty());
    }
}
