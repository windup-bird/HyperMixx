//! 控制总线（mixxx ControlObject 思想的 Rust 版）：
//! UI / MIDI / 音频引擎之间唯一的通信面。
//!
//! - 读无锁：seqlock，音频回调每块批量读一次，永不阻塞；
//! - 写极罕见（UI/MIDI 事件率），seqlock 写锁短暂持有；
//! - `generation` 计数器供 UI 轮询检测变更（30Hz 定时器比对）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use seqlock::SeqLock;

struct Control {
    value: SeqLock<f64>,
    generation: AtomicU64,
}

impl Control {
    fn new(value: f64) -> Self {
        Self {
            value: SeqLock::new(value),
            generation: AtomicU64::new(0),
        }
    }

    fn get(&self) -> f64 {
        // seqlock 0.2 的 read 在写者活跃时内部让出重试，读侧永远无锁。
        self.value.read()
    }

    fn set(&self, value: f64) {
        // UI/MIDI 轮询写入常见：值未变时跳过，避免无谓的 seqlock 写与代次递增
        if self.value.read() == value {
            return;
        }
        *self.value.lock_write() = value;
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
}

/// 克隆开销极小的控制句柄（Arc 共享），可传给任意线程。
#[derive(Clone)]
pub struct ControlHandle(Arc<Control>);

impl ControlHandle {
    pub fn get(&self) -> f64 {
        self.0.get()
    }

    pub fn set(&self, value: f64) {
        self.0.set(value);
    }

    /// 值被写入的次数；UI 轮询用。
    pub fn generation(&self) -> u64 {
        self.0.generation()
    }
}

/// 控制点注册表。path 形如 `"Deck1.play"`、`"Master.crossfader"`。
/// Clone 共享同一个注册表（底层 Arc），可安全传给 UI / 引擎 / 分析线程。
#[derive(Clone, Default)]
pub struct ControlBus {
    controls: Arc<RwLock<HashMap<String, Arc<Control>>>>,
}

impl ControlBus {
    /// 获取控制句柄；不存在则以初值 0.0 创建。
    pub fn control(&self, path: &str) -> ControlHandle {
        if let Ok(map) = self.controls.read()
            && let Some(c) = map.get(path)
        {
            return ControlHandle(c.clone());
        }
        let mut map = self.controls.write().unwrap();
        ControlHandle(
            map.entry(path.to_string())
                .or_insert_with(|| Arc::new(Control::new(0.0)))
                .clone(),
        )
    }

    /// 设置并返回句柄。
    pub fn set(&self, path: &str, value: f64) -> ControlHandle {
        let h = self.control(path);
        h.set(value);
        h
    }

    pub fn get(&self, path: &str) -> f64 {
        self.control(path).get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn set_get_roundtrip() {
        let bus = ControlBus::default();
        let h = bus.control("Deck1.play");
        assert_eq!(h.get(), 0.0);
        h.set(1.0);
        assert_eq!(h.get(), 1.0);
        // 同路径返回同一底层值
        assert_eq!(bus.get("Deck1.play"), 1.0);
    }

    #[test]
    fn generation_increments_on_set() {
        let bus = ControlBus::default();
        let h = bus.control("Master.volume");
        let g0 = h.generation();
        h.set(0.5);
        let g1 = h.generation();
        h.set(0.6);
        let g2 = h.generation();
        assert!(g0 < g1 && g1 < g2);
    }

    #[test]
    fn concurrent_reads_are_sane() {
        let bus = ControlBus::default();
        let h = bus.control("Deck1.rate");
        let writer_h = h.clone();
        let writer = thread::spawn(move || {
            for i in 0..10_000 {
                writer_h.set(i as f64 / 100.0);
            }
        });
        let mut readers = Vec::new();
        for _ in 0..4 {
            let h = h.clone();
            readers.push(thread::spawn(move || {
                let mut last = f64::NEG_INFINITY;
                for _ in 0..100_000 {
                    let v = h.get();
                    assert!(v.is_finite());
                    last = last.max(v);
                }
                last
            }));
        }
        writer.join().unwrap();
        for r in readers {
            let _ = r.join().unwrap();
        }
        // 最终值是某个已写入的值
        assert!(h.get() >= 0.0 && h.get() < 100.0);
    }
}
