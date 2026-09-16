//! 打印默认输出设备与其支持的采样率范围,用于诊断播放速度问题。

use cpal::traits::{DeviceTrait, HostTrait};

fn main() {
    let host = cpal::default_host();
    println!("host: {:?}", host.id());
    let Some(device) = host.default_output_device() else {
        println!("没有输出设备(headless)");
        return;
    };
    println!("device: {}", device.name().unwrap_or_default());
    match device.default_output_config() {
        Ok(cfg) => println!(
            "default: {} Hz / {} ch / {:?}",
            cfg.sample_rate().0,
            cfg.channels(),
            cfg.sample_format()
        ),
        Err(e) => println!("default config 读取失败: {e}"),
    }
    if let Ok(range) = device.supported_output_configs() {
        for r in range {
            println!(
                "  range: {}..{} Hz / {} ch / {:?}",
                r.min_sample_rate().0,
                r.max_sample_rate().0,
                r.channels(),
                r.sample_format()
            );
        }
    }
}
