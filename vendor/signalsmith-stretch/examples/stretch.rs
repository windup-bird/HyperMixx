use std::fs::File;
use std::path::PathBuf;

use anyhow::{anyhow, bail};
use clap::Parser;
use cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};
use cpal::FromSample;

#[derive(Parser, Debug)]
#[command(name = "stretch")]
struct Args {
    /// The file to play.
    file: PathBuf,
    /// The time stretch factor.
    #[arg(long, default_value = "1")]
    rate: f32,
    /// The pitch adjustment, in semitones.
    #[arg(long, default_value = "0")]
    semitones: f32,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let file = File::open(args.file)?;
    let wav_reader = hound::WavReader::new(file)?;
    let spec = wav_reader.spec();

    let host = cpal::default_host();

    let device = host
        .default_output_device()
        .expect("Failed to find a default output device");
    let config = device
        .supported_output_configs()?
        .find(|conf| {
            conf.channels() == spec.channels
                && conf.min_sample_rate().0 <= spec.sample_rate
                && conf.max_sample_rate().0 >= spec.sample_rate
                && conf.sample_format() == cpal::SampleFormat::F32
        })
        .ok_or(anyhow!("failed to configure output device"))?;

    let config = config.with_sample_rate(cpal::SampleRate(spec.sample_rate));

    match (spec.bits_per_sample, spec.sample_format) {
        (16, hound::SampleFormat::Int) => play::<i16, _>(
            wav_reader,
            &device,
            args.rate,
            args.semitones,
            config.into(),
        ),
        (32, hound::SampleFormat::Float) => play::<f32, _>(
            wav_reader,
            &device,
            args.rate,
            args.semitones,
            config.into(),
        ),
        (depth, format) => bail!("unsupported sample format: {depth}bit {format:?}"),
    }
}

fn play<T, R>(
    file: hound::WavReader<R>,
    device: &cpal::Device,
    rate: f32,
    semitones: f32,
    output_config: cpal::StreamConfig,
) -> anyhow::Result<()>
where
    T: hound::Sample + Send + 'static,
    R: std::io::Read + Send + 'static,
    f32: FromSample<T>,
{
    let mut samples = file
        .into_samples::<T>()
        .filter_map(Result::ok)
        .map(f32::from_sample_);

    let (done_tx, done_rx) = oneshot::channel();
    let mut done_once = Some(done_tx);

    let sample_rate = output_config.sample_rate.0;
    let channels = output_config.channels;
    let mut stretch = signalsmith_stretch::Stretch::preset_default(channels as u32, sample_rate);
    stretch.set_transpose_factor_semitones(semitones, None);

    let mut input_buffer = Vec::new();

    let stream = device.build_output_stream(
        &output_config,
        move |output: &mut [f32], _: &cpal::OutputCallbackInfo| {
            // Read input from the wav file, filling a buffer that is
            // proportionally larger or smaller than the output depending on
            // rate.
            let input_len = (output.len() as f32 * rate) as usize;
            input_buffer.resize(input_len.next_multiple_of(channels as usize), 0.0);

            for sample in input_buffer.iter_mut() {
                *sample = samples.next().unwrap_or_else(|| {
                    if let Some(done) = done_once.take() {
                        let _ = done.send(());
                    };

                    0.0
                })
            }

            // Now we're ready to stretch.
            stretch.process(&input_buffer, output);
        },
        |err| eprintln!("an error occurred on stream: {}", err),
        None,
    )?;

    stream.play()?;
    let _ = done_rx.recv();

    Ok(())
}
