//! 音轨解码（symphonia 0.6，纯 Rust）与 44.1k→48k 重采样（rubato 5.0）。
//! 只在 worker 线程使用，不进实时路径。

use std::path::Path;

use anyhow::{Result, anyhow};
use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Time;

use rubato::Resampler as _;

/// 一次重采样的输入块长（帧）。
pub const RESAMPLE_INPUT_CHUNK: usize = 2048;

pub struct TrackDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    pub sample_rate: u32,
    pub channels: usize,
    pub n_frames: Option<u64>,
}

impl TrackDecoder {
    pub fn open(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let format = symphonia::default::get_probe().probe(
            &Hint::new(),
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )?;
        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| anyhow!("没有可解码的音轨"))?;
        let track_id = track.id;
        let n_frames = track.num_frames;
        let Some(CodecParameters::Audio(params)) = track.codec_params.as_ref() else {
            return Err(anyhow!("音轨没有音频编码参数"));
        };
        let sample_rate = params.sample_rate.unwrap_or(44_100);
        let channels = params
            .channels
            .as_ref()
            .map(|c| c.count().clamp(1, 2))
            .unwrap_or(2);
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())?;
        Ok(Self {
            format,
            decoder,
            track_id,
            sample_rate,
            channels,
            n_frames,
        })
    }

    /// 解出下一段交织立体声 f32（原始采样率）。EOF → None。
    pub fn decode_next(&mut self) -> Result<Option<Vec<f32>>> {
        loop {
            let packet = match self.format.next_packet()? {
                Some(p) => p,
                None => return Ok(None),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            let decoded: GenericAudioBufferRef = match self.decoder.decode(&packet) {
                Ok(d) => d,
                Err(SymError::DecodeError(_)) | Err(SymError::IoError(_)) => continue,
                Err(e) => return Err(e.into()),
            };
            let frames = decoded.frames();
            let in_ch = decoded.spec().channels().count();
            let mut interleaved = vec![0.0f32; frames * in_ch.max(1)];
            decoded.copy_to_slice_interleaved::<f32, _>(&mut interleaved);
            // 转立体声交织
            let mut out = Vec::with_capacity(frames * 2);
            match in_ch {
                1 => {
                    for s in &interleaved {
                        out.push(*s);
                        out.push(*s);
                    }
                }
                _ => {
                    for f in interleaved.chunks_exact(in_ch) {
                        out.push(f[0]);
                        out.push(f[1]);
                    }
                }
            }
            if !out.is_empty() {
                return Ok(Some(out));
            }
        }
    }

    /// 跳到指定秒数（原始时间轴）。
    pub fn seek(&mut self, seconds: f64) -> Result<()> {
        let time = Time::try_from_secs_f64(seconds.max(0.0))
            .ok_or_else(|| anyhow!("非法跳转时间: {seconds}"))?;
        self.format.seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time,
                track_id: Some(self.track_id),
            },
        )?;
        self.decoder.reset();
        Ok(())
    }
}

/// 增量式重采样器：原始率 → 48kHz，输入输出均为交织立体声 f32。
/// 内部把不足一个输入 chunk 的余量缓存，凑满再交给 rubato。
pub struct To48k {
    rs: Option<rubato::Async<f32>>,
    sr_in: u32,
    sr_out: u32,
    carry: Vec<f32>,
}

impl To48k {
    pub fn new(sr_in: u32, sr_out: u32) -> Result<Self> {
        let rs = if sr_in == sr_out {
            None
        } else {
            let params = rubato::SincInterpolationParameters {
                sinc_len: 256,
                f_cutoff: Some(0.95),
                oversampling_factor: 256,
                interpolation: rubato::SincInterpolationType::Linear,
                window: rubato::WindowFunction::BlackmanHarris2,
            };
            Some(rubato::Async::new_sinc(
                sr_out as f64 / sr_in as f64,
                1.2,
                &params,
                RESAMPLE_INPUT_CHUNK,
                2,
                rubato::FixedAsync::Input,
            )?)
        };
        Ok(Self {
            rs,
            sr_in,
            sr_out,
            carry: Vec::new(),
        })
    }

    /// 处理一段交织立体声（原始率），返回交织立体声（目标率）。
    pub fn process(&mut self, input: &[f32]) -> Result<Vec<f32>> {
        let Some(rs) = self.rs.as_mut() else {
            return Ok(input.to_vec());
        };
        self.carry.extend_from_slice(input);
        let chunk = rs.input_frames_next();
        let mut out = Vec::new();
        while self.carry.len() >= chunk * 2 {
            let take: Vec<f32> = self.carry.drain(..chunk * 2).collect();
            out.extend_from_slice(&resample_chunk(rs, &take, chunk, None)?);
        }
        Ok(out)
    }

    /// EOF：处理残余帧（不足一 chunk 时补零 + partial_len），
    /// 内部 sinc 延迟（约 3ms）直接放弃，换取精确的结束位置。
    pub fn flush(&mut self) -> Result<Vec<f32>> {
        let Some(rs) = self.rs.as_mut() else {
            return Ok(std::mem::take(&mut self.carry));
        };
        if self.carry.is_empty() {
            return Ok(Vec::new());
        }
        let n_frames = self.carry.len() / 2;
        let chunk = rs.input_frames_next();
        let mut take = std::mem::take(&mut self.carry);
        take.resize(chunk * 2, 0.0);
        resample_chunk(rs, &take, chunk, Some(n_frames))
    }

    pub fn source_rate(&self) -> u32 {
        self.sr_in
    }
    pub fn target_rate(&self) -> u32 {
        self.sr_out
    }
}

fn resample_chunk(
    rs: &mut rubato::Async<f32>,
    interleaved: &[f32],
    chunk: usize,
    partial: Option<usize>,
) -> Result<Vec<f32>> {
    let l: Vec<f32> = interleaved.iter().step_by(2).copied().collect();
    let r: Vec<f32> = interleaved.iter().skip(1).step_by(2).copied().collect();
    let planes = [l, r];
    let input =
        rubato::audioadapter_buffers::direct::SequentialSliceOfVecs::new(&planes, 2, chunk)?;
    let indexing = partial.map(|n| rubato::Indexing::new().partial_len(n));
    let resampled = rs.process(&input, indexing.as_ref())?;
    Ok(resampled.take_data())
}
