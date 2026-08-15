//! 音频输出后端抽象（trait 缝：默认 cpal，可替换 JACK/其它）。

use anyhow::{Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

pub struct DeviceInfo {
    pub name: String,
}

pub trait AudioStream: Send {
    fn play(&self) -> Result<()>;
    fn pause(&self);
}

/// 打开输出流的回调：`cb(out)`，`out` 为交织立体声 f32，长度 = frames*2。
pub type StreamCallback = Box<dyn FnMut(&mut [f32]) + Send>;

pub trait AudioBackend: Send {
    fn devices(&self) -> Vec<DeviceInfo>;

    /// 打开输出流。`device` 为设备名（None = 系统默认）。
    fn open(
        &self,
        device: Option<&str>,
        sample_rate: u32,
        frames: usize,
        callback: StreamCallback,
    ) -> Result<Box<dyn AudioStream>>;
}

/// 基于 cpal 的默认实现（ALSA / JACK / PipeWire / PulseAudio）。
pub struct CpalBackend {
    host: cpal::Host,
}

impl CpalBackend {
    pub fn new() -> Result<Self> {
        let host = cpal::default_host();
        Ok(Self { host })
    }
}

impl AudioBackend for CpalBackend {
    fn devices(&self) -> Vec<DeviceInfo> {
        let mut out = Vec::new();
        if let Ok(devs) = self.host.output_devices() {
            for d in devs {
                out.push(DeviceInfo {
                    name: d.to_string(),
                });
            }
        }
        out
    }

    fn open(
        &self,
        device: Option<&str>,
        sample_rate: u32,
        frames: usize,
        mut callback: StreamCallback,
    ) -> Result<Box<dyn AudioStream>> {
        let dev = match device {
            Some(name) => self.host.output_devices()?.find(|d| d.to_string() == name),
            None => self.host.default_output_device(),
        }
        .ok_or_else(|| anyhow!("找不到输出设备（{}）", device.unwrap_or("default")))?;

        let dev_name = dev.to_string();
        let supported: Vec<_> = dev.supported_output_configs()?.collect();
        if supported.is_empty() {
            return Err(anyhow!("设备 {dev_name} 无输出配置"));
        }

        // 选配置：必须支持 48kHz + 立体声 + F32（引擎采样率固定 48k）
        let range = supported
            .iter()
            .find(|c| {
                c.channels() == 2
                    && c.sample_format() == cpal::SampleFormat::F32
                    && c.min_sample_rate() <= sample_rate
                    && sample_rate <= c.max_sample_rate()
            })
            .ok_or_else(|| {
                anyhow!(
                    "设备 {dev_name} 不支持 {sample_rate}Hz 立体声 F32，可用的配置: {}",
                    supported
                        .iter()
                        .map(|c| format!(
                            "{}-{}Hz/{:?}/{}ch",
                            c.min_sample_rate(),
                            c.max_sample_rate(),
                            c.sample_format(),
                            c.channels()
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
        let cfg = range
            .try_with_sample_rate(sample_rate)
            .ok_or_else(|| anyhow!("无法锁定 {sample_rate}Hz 配置"))?;
        let buffer_size = match cfg.buffer_size() {
            cpal::SupportedBufferSize::Range { min, max }
                if *min <= frames as u32 && frames as u32 <= *max =>
            {
                cpal::BufferSize::Fixed(frames as u32)
            }
            _ => cpal::BufferSize::Default,
        };
        let config = cpal::StreamConfig {
            channels: 2,
            sample_rate,
            buffer_size,
        };
        log::info!(
            "打开输出设备 {dev_name}：{}Hz, {:?}",
            sample_rate,
            buffer_size
        );

        // 回调线程优先级（尽力而为，失败无害）
        let mut prio_done = false;
        let stream = dev.build_output_stream(
            config,
            move |data: &mut [f32], _info| {
                if !prio_done {
                    prio_done = true;
                    crate::dsp::enable_ftz();
                    bump_thread_priority();
                }
                callback(data);
            },
            |err| log::error!("音频输出错误: {err}"),
            None,
        )?;
        Ok(Box::new(CpalStream { stream }))
    }
}

struct CpalStream {
    stream: cpal::Stream,
}

impl AudioStream for CpalStream {
    fn play(&self) -> Result<()> {
        self.stream.play()?;
        Ok(())
    }
    fn pause(&self) {
        let _ = self.stream.pause();
    }
}

/// 提高当前线程优先级（Linux：per-thread nice；需要 CAP_SYS_NICE，失败无害）。
fn bump_thread_priority() {
    #[cfg(target_os = "linux")]
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::pid_t;
        let _ = libc::setpriority(libc::PRIO_PROCESS, tid as u32, -10);
    }
}
