//! HyperMixx 入口：控制总线 + 音频引擎 + Slint UI。
//! 用法: hypermixx-app [曲目1] [曲目2]

fn main() -> anyhow::Result<()> {
    env_logger::init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let controls = hypermixx_core::ControlBus::default();
    let backend = hypermixx_audio::CpalBackend::new()?;
    let engine = hypermixx_audio::Engine::start(&backend, &controls, None)?;

    let ui = hypermixx_ui::Ui::new(controls, engine.handle)?;

    // 命令行曲目经 UI 通道加载（引擎 + 波形分析一起启动）
    for (i, path) in args.iter().take(hypermixx_audio::Engine::DECKS).enumerate() {
        ui.load_track(i, path.into());
    }

    ui.run()
}
