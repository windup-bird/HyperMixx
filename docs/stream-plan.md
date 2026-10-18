# 网络串流调研与实施计划

> 调研日期：2026-10-10。目标：通过 web 获取音频/视频并加载到 deck、网易云音乐 /
> SoundCloud 等流媒体播放，以及歌单获取与播放队列。结论先行：**纯 Rust 可行（网易云
> 成本低），歌单只需 net + cli 层（不动引擎），SoundCloud 因官方砍掉 progressive 而
> 必须做 HLS，是全计划最大的技术风险点。**

---

## 1. 本项目的硬约束（决定实现路径）

### 1.1 `Source` 是随机访问接口

`crates/hypermixx-core/src/source.rs`：

```rust
pub trait Source: Send + Sync {
    fn read_frames(&self, start_frame: u64, output: &mut [f32]) -> usize;
    fn total_frames(&self) -> u64;
}
```

deck 的 cue / jump / beatjump / loop / sync、波形峰值、beat 网格分析、stem 分离**全部依赖
"整轨 PCM 在内存 + 长度已知 + 任意位置可读"**。现有 `PcmPool` 就是 `Arc<Vec<f32>>`。

### 1.2 读取发生在 producer（mixer）线程

`crates/hypermixx-audio/src/flow/pitchshift.rs::feed()` 直接调 `source.read_frames`：

- 会阻塞网络的 Source ⇒ mixer 停摆 ⇒ 音频中断；
- 短读（返回 0）⇒ `read == 0` → 跳出喂入 → **出静音间隙，但不崩**（`MAX_FEED_BATCHES`
  循环内 `if read == 0 { break }`，下一块重试）；
- 因此渐进式 Source **必须非阻塞**，缓冲外的读返回 0。

### 1.3 加载已是后台线程 + 进度通知

`crates/hypermixx-cli/src/command.rs::spawn_load`：

```
thread::spawn → hypermixx_media::decode_file(path) → PcmPool
             → Command::Load { deck_id, source, analysis }
             → notices::info / UiEvent::Loaded
```

"URL → 下载 → 解码" 可以直接插在这个流程前面，`analyse`/`stem`/波形对完整下载的曲目
**零改动可用**。

### 1.4 已有 HTTP 栈与缓存模式

- `ureq` 3 已在工作区（`hypermixx-stems` 的 `onnx` feature 用它下载模型）；
- `hypermixx-stems/src/model.rs` 提供可照抄的模式：进度回调 + 临时文件 + 原子改名 +
  `.sha256` marker + `HYPERMIXX_MODELS`/`HYPERMIXX_CACHE` 环境变量（`model::models_root()` / `cache::cache_root()`）；
- **工作区无 async runtime**，全部是线程模型 —— 网络层应保持阻塞式 ureq + worker 线程。

### 1.5 解码格式现状（workspace `Cargo.toml` 的 symphonia features）

已启用：`mp3 mp1 mp2 wav aiff flac pcm isomp4 aac ogg vorbis`
未启用：`mkv`、`opus`（无 MPEG-TS、无 HLS、无 ADTS 显式入口）

- 直链 mp3 / wav / flac / m4a(aac) ✓ 开箱即用；
- `.webm`（matroska + opus）✗，加 `mkv` + `opus` 两个 feature 即可（一行改动）；
- **HLS 分段（TS / fMP4）✗，需要额外工作**（见 §3.2）；
- symphonia 0.5.4 **自带 `AdtsReader`**（`symphonia-codec-aac`，`aac` feature 已启用），
  即裸 ADTS AAC 流可以直接 probe + 解码 —— 这是 SoundCloud AAC-HLS 的一条现成出路。

### 1.6 内存与"边下边播"

引擎域 44.1 kHz 立体声 f32 ⇒ 约 352.8 KB/s，7 分钟曲目 ≈ **140 MiB**。
整轨本来就全在内存，**渐进式加载不省内存，只省起播等待**。

---

## 2. 外部调研

### 2.1 yt-dlp 能力（读的是上游 `yt_dlp/extractor/neteasemusic.py` 与 `soundcloud.py`）

**网易云（`netease:song` / `netease:playlist` / `netease:album` / `netease:singer` /
`netease:program` / `netease:djradio` / `netease:mv`）**

- 直链走 `POST https://interface3.music.163.com/eapi/song/enhance/player/url/v1`，
  **eapi 加密 = AES-128-ECB（固定密钥 `e82ckenh8dichen8`）+ MD5 签名**，请求体
  `{"ids": "[id]", "level": ..., "encodeType": "flac"}`，外加一串伪造设备 cookie
  （`os=pc`、`appver`、`deviceId`、`requestId` 等），仅 VIP 时才带 `MUSIC_U`；
- 元数据走明文 `GET music.163.com/api/song/detail?id=…`（带 Referer），
  歌词走 `api/song/lyric`；
- 音质档位：

  | level | 规格 | 权限 |
  |---|---|---|
  | standard / higher / exhigh | 128 / 192 / 320 kbps mp3(或 aac) | **免费，无需登录** |
  | lossless / hires | 48k/16bit、192k/24bit flac | VIP |
  | jyeffect / jymaster / sky | 空间音效 / 母带 / 沉浸环绕 flac | SVIP |

- 风控返回 `code == -462` ⇒ 要求登录；部分曲目**仅中国大陆可解析**（上游测试标注
  "Blocked outside Mainland China"）。

**SoundCloud**

- `client_id` 自动发现：首页 → 逆序找 `<script src>` → 正则 `client_id\s*:\s*"([0-9a-zA-Z]{32})"`
  → 缓存；API 返回 401/403 时自动刷新重试；
- 从 `media.transcodings` 选格式，id 形如 `http_aac / hls_aac / http_mp3 / hls_mp3 /
  http_opus / hls_opus`，`snipped` / `/preview/` 标记试听片段；
- 免费用户 96k/160k AAC，Go+ 才有 256k；公开曲目**不需要登录**，私有链接需 oauth token
  或 cookies；限速约 600 请求 / 10 分钟（429 退避）；
- yt-dlp 下载 HLS **需要 ffmpeg**（分段转封装）。

**一句话**：覆盖面最强、站点改版由上游维护、免登录拿免费音质；但它是外部运行时依赖
（Python/二进制，HLS 还要 ffmpeg），与"纯 Rust"取向冲突 —— 尤其 SoundCloud 这一站
无论如何都绕不开 HLS。

### 2.2 SoundCloud 官方时间线（`github.com/soundcloud/api` issue #441，读到 2026-10-08）

- 2025-09 公告：转 AAC HLS（`hls_aac_160_url` / `hls_aac_96_url`），
  `http_mp3_128_url` / `hls_mp3_128_url` / `hls_opus_64_url` 弃用；
- 2026-04-22：不升采样，原文件 <160kbps 的曲目以 **96k AAC** 为准；
- 2026-08-05：多数目录已转码，存量用户曲目可能**一年内仍只有 MP3** ⇒
  "先找 AAC、再找 MP3（内部客户端同样做法）"；
- **2026-08-12 release：progressive MP3 已删除**；MP3-HLS 暂留至迁移完成；
- 2026-09-29：AAC 是唯一长期支持的流式格式；
- api-v2 官方声明"不供第三方使用"（社区事实标准，灰色地带，README 需注明）。

**结论：SoundCloud 今天已经没有 progressive 直链，纯 Rust 必须实现 HLS。**

### 2.3 纯 Rust 生态件

| 需求 | 候选 | 备注 |
|---|---|---|
| HTTP（阻塞） | `ureq` 3 | 已在工作区，零新增 |
| HLS 播放列表解析 | `m3u8-rs`（6.x，下载量最大）/ `hls_m3u8` | 只解析，不含下载 |
| MPEG-TS 解复用 | `mpeg2ts-reader`（零拷贝，需自写 PacketFilter 抽 PES）/ `mpeg2ts` / `mpeg-ts-core` | TS 分段才需要 |
| ADTS 帧切分 | `adts-reader` 或 symphonia 自带 `AdtsReader` | 后者已可用 |
| 网易云原生 SDK | `ncm-api-rs`（EAPI/WEAPI 全实现，300+ 接口）/ `ncmapi2` / `netease-qq-music-api` | 都绑 tokio+reqwest，与现有线程模型不同代 |
| SoundCloud 原生 SDK | `soundcloud-rs`（自动发现 client_id）、`rsoundcloud` | 均为 0.x 小项目 |
| HLS 合并/转封装兜底 | 外部 `ffmpeg`（本机已有 `/usr/bin/ffmpeg`） | 做成可选 feature |

**关于 `ncm-api-rs`**：接口最全但引入 tokio + reqwest。本计划只需要 2~3 个端点，
建议**自研最小 eapi 客户端**（`aes` + `md-5` + `ureq` + `serde_json`，约百余行，
yt-dlp 源码可直接对照），保持零 async。

---

## 3. 分服务实现路线

### 3.1 直链 http(s) —— Phase 1

```
resolve(url) → 直接当直链（HEAD/GET，取 Content-Length 与 Content-Type）
            → fetch 到 ./.cache/media/<sha1>.<ext>（原子写 + meta）
            → decode_file → PcmPool → Command::Load
```

- 时长未知时以 `Content-Length` 推断或退回"下载完再 Load"；
- 参照 `stems/model.rs::download_to` 的进度与 marker 写法。

### 3.2 网易云 —— Phase 2（纯 Rust，成本低）

```
元数据: GET  music.163.com/api/song/detail?id=X&ids=[X]      (明文, Referer)
直链:   POST interface3.music.163.com/eapi/song/enhance/player/url/v1
        params = hex(AES-128-ECB(pkcs7("path-36cd479b6b5-json-md5"), key=e82ckenh8dichen8))
        → data[0].{url, br, size, type, level}
```

- level 缺省 `exhigh`（320k，免费档上限），可配 `lossless` 等 + `MUSIC_U` cookie 文件；
- 免费曲目无需登录；VIP/风控（-462）/区域限制作为可诊断错误上抛到 notices；
- 搜索 `api/search/get`（可选，Phase 2 之后）。

### 3.3 SoundCloud —— Phase 4（HLS，最大风险）

```
resolve(url) → api-v2.soundcloud.com/resolve?url=…&client_id=…
            → media.transcodings 中选流（优先 hls_aac，回退 hls_mp3）
            → transcoding url + client_id → 真实 playlist.m3u8
            → 解析分段 → 并发下载 → 拼接/转封装 → 喂给解码管线
```

分段格式三选一（**必须先实测**，写个小测试拉真实 m3u8 判定）：

1. **裸 MP3 分段**（`cf-hls-media…/playlist/<id>.128.mp3/playlist.m3u8` 很可能如此）
   → 直接拼接 → symphonia mp3 ✓（成本最低）；
2. **MPEG-TS 分段**（AAC 常见）→ `mpeg2ts-reader` 抽 PES → ADTS/MP3 帧
   → symphonia `AdtsReader` / mp3 ✓；
3. **fMP4 分段**（带 `EXT-X-MAP`）→ init + frag 拼接 → symphonia isomp4
   （fragmented 支持未验证，失败则退到 TS 路径或 ffmpeg）。

逃生舱：`hls-ffmpeg` feature —— 检测到纯 Rust 路径不可用时调用
`ffmpeg -i <m3u8> -f mp3 pipe:1`（模式与 stems 的 `cuda` feature 一致）。

### 3.4 视频来源

本软件是纯音频 CLI/TUI，**只提取音轨**：解析时优先选纯音频格式
（`bestaudio[ext=m4a]` 一类），`.webm` 需加 symphonia `mkv`+`opus` feature。
不做视频画面层。

---

## 4. 边下边播（Phase 3）设计

### 4.1 前提

`total_frames` 必须在 Load 前确定。三个来源都能给时长：
网易云 `duration`、SoundCloud `duration`、直链 `Content-Length`。
**拿不到时长的源退回"下载完再 Load"。**

### 4.2 `ProgressiveSource`

```rust
pub struct ProgressiveSource {
    // 分块存储 + 原子长度：读路径无锁，写只在追加块时短暂持锁
    chunks: RwLock<Vec<Arc<[f32]>>>,   // 或 Vec<Arc<[f32; CHUNK]>> + AtomicUsize
    total: u64,                        // 由元数据时长 × SAMPLE_RATE 得出
    buffered: AtomicU64,               // 已可用帧数
    finished: AtomicBool,
    failed: Mutex<Option<String>>,
}
impl Source for ProgressiveSource {
    fn read_frames(&self, start, out) -> usize {
        // start >= buffered → 返回 0（非阻塞，见 §1.2）
        // 否则拷贝 min(want, buffered - start)
    }
    fn total_frames(&self) -> u64 { self.total }
}
```

### 4.3 管线

```
[下载线程] ureq 流式读 chunk
    → [解码线程] symphonia（自定义 MediaSource：阻塞 recv，只跑在自己线程）
        → 追加 PCM 到 ProgressiveSource
            → 缓冲 ≥ 2s（> flow 预热量 32768 帧 ≈ 0.74s）→ Command::Load
            → 之后保持领先播放头 30s+
```

`media::decode_file` 需拆出可复用的 `Decoder`（包 symphonia 的
probe/track 选择/decode 循环，输入换成"可增长的 `MediaSource`"）。

### 4.4 语义与 gate

- **`analyse` / `stem` / 波形**：`finished == false` 时拒绝并提示"仍在下载"；
- **`loop` / `beatjump` / `jump` 目标 > buffered**：前端直接拒绝（避免跳进静音区）；
- **网络抖动**：`feed()` 短读 ⇒ 静音间隙、位置继续走 —— 靠"领先 30s"把概率压到可忽略；
- **缓存不足时长未知**：不进渐进模式，走 Phase 1 的完整下载。

### 4.5 必须先验证的风险点

1. `feed()` 短读时 deck 位置是否继续推进（读代码判断是"出静音 + 位置继续"，需写测试确认）；
2. `Flow::prepare` 预热遇到短读的行为（所以 Load 门槛要 ≥ 预热量）；
3. `total_frames` 为估计值时与实际 PCM 长度不一致的收尾处理（下载完成时校正）。

---

## 5. 分阶段实施计划

### Phase 1 — 直链 URL 加载（地基，估 1~2 天）

- 新 crate **`hypermixx-net`**：依赖 `hypermixx-core` / `hypermixx-media` + `ureq` +
  `serde_json`，与现有线程模型无冲突；
  - `resolve(spec) -> Resolved { direct_url, title, ext, duration, headers }`
    （本期只认 `http(s)://` 直链）；
  - `fetch(url, dst, progress)` → `./.cache/media/<sha1>.<ext>` +
    `.meta.json`（照抄 stems 的临时文件 + 原子改名 + marker 模式）；
- `hypermixx-cli`：`load` 见到 `://` 走 `spawn_stream_load`
  （resolve → fetch 进度 notices → `decode_file` → `Command::Load`）；
- TUI 文件浏览器增加"输入 URL"入口；补全器认识 URL 形态；
- 可选命令 `cache list|clear`。

验收：`load https://…/x.mp3` 有进度、二次加载命中缓存 0s、`analyse`/`stem` 照常。

### Phase 2 — 服务解析器：网易云 + SoundCloud 元数据（估 2~4 天）

- `hypermixx-net/src/services/netease.rs`：eapi cipher（含已知向量单测）+
  `song/detail` + `player/url/v1` + 可选 `MUSIC_U` cookie 文件；
- `hypermixx-net/src/services/soundcloud.rs`：`client_id` 发现与缓存、`resolve` →
  `transcodings` 选流、401/403 自动刷新、429 退避（SoundCloud 的流落地依赖 Phase 4）；
- 命令面：`load music.163.com/#/song?id=…`、`load soundcloud.com/…`、
  `load ncm:<id>`；缓存 key 用曲目 id 而非 URL，meta 存 title/duration/source；
- 后续可选：`search ncm|sc <关键词>` → 列表 → `load <n>`。

验收：网易云免费曲目（128/192/320k）全链路可播；VIP 缺 cookie 时错误信息可诊断。

### Phase 2.5 — 歌单与播放队列（估 2~3 天，不依赖 Phase 3/4）

数据层（`hypermixx-net`）：

```rust
pub struct Playlist { id, title, service, tracks: Vec<TrackRef> }
pub struct TrackRef { id, title, artists, duration_ms, service }  // 1000 首 ≈ 几 KB
fn resolve_playlist(spec) -> Playlist;   // 只拉 id/标题列表
fn resolve_track(&TrackRef) -> Resolved; // 复用 Phase 2；**播放时才懒解析**
```

**懒解析是硬要求**：每首的直链都要单独请求且会过期，队列里只存 `TrackRef`。

歌单接口（2026-10 核实）：

| 服务 | 需求 | 端点 | 关键坑 |
|---|---|---|---|
| 网易云 | 公开歌单详情 | eapi `/eapi/v3/playlist/detail?id=` | **`tracks` 永远不完整**（未登录仅 ~10 首），**`trackIds` 完整**；全量元数据需按 500/批请求 `song/detail`，或用 `/playlist/track/all`（limit+offset 分页，>1000 首必须显式分页） |
| 网易云 | 搜索歌单 | `/api/search/get`，`type=1000` | 明文，无需登录 |
| 网易云 | **我的歌单** / 日推 / 收藏 | `/api/user/playlist?uid=`、`/recommend/songs` | **必须登录**（`MUSIC_U`；登录后请求要带 `timestamp` 否则判非法） |
| 网易云 | 专辑/歌手/电台 | `album?` / `artist?` / `djradio?` | yt-dlp 同族接口，可顺带支持 |
| SoundCloud | 公开 set（`user/sets/slug`） | `api-v2/resolve?url=` | 返回的 `tracks` 常是**残缺 stub**，需再打 `tracks?ids=…&playlistId=…` 批量补全（yt-dlp extractor 同样处理） |
| SoundCloud | 用户页/曲目/收藏 | `users/{id}/playlists\|tracks\|likes` | `linked_partitioning=1&limit=200` 分页，429 限速 600/10min |
| SoundCloud | 我自己的歌单 | `/me/playlists` 等 | **需 oauth token**（非 client_id） |

播放语义（**默认取推荐项，待用户确认**，见 §7）：

- **per-deck 队列**（DJ 软件惯例，Serato 的"自动加载下一曲"即 per-deck）：
  deck 持有 `Vec<TrackRef>` + 游标；
- 命令族：
  - `queue load <歌单URL|ncm:id|sc:…>` — 载入队列（可选立即载入第 1 首）
  - `queue list` / `queue next [n]` / `queue prev` / `queue jump <n>`
  - `queue shuffle` / `queue repeat off|one|all` / `queue clear`
  - `deck0 queue autoplay on` — 接近尾声时后台预取下一曲
- TUI：队列面板（`第 12/240 首 · 标题`）+ 键位切曲，`queue load` 复用现有 picker；
- **`Command::Load` 会重建 flow，不是无缝换曲**（与 deck 内 `set_sources` 的换源 jump
  不同）：第一版接受"播完 → 自动 Load 下一曲"的硬切，无缝化留作后续优化。

磁盘缓存补 **LRU**：歌单显著放大缓存量（1000 首 × 5 MB ≈ 5 GB），
"永久缓存"的假设不再成立，需加上限与淘汰策略（`cache list|clear|limit`）。

验收：公开网易云歌单/专辑 URL 载入后 `queue list` 可见全量曲目；`next`/`prev`/`jump`
可切曲并正常播放；缓存超限自动淘汰。

### Phase 3 — 边下边播（估 3~5 天）

- `media`：拆出 `Decoder`（增量输入）；
- `hypermixx-net/progressive.rs`：`ProgressiveSource` + 下载/解码双线程 + 预缓冲门限；
- CLI/TUI：`state` 显示缓冲进度徽标；`analyse`/`stem`/波形按 §4.4 gate；
- 先写 §4.5 三个风险点的测试再合入。

验收：直链 mp3 起播延迟 ≈ 预缓冲时长；播放中网络中断只出现间隙不崩；
跳转/循环被正确拒绝或限幅；下载完成后分析/stems 全部可用。

### Phase 4 — SoundCloud HLS（估 2~4 天，含实测）

- 实测分段格式（裸 MP3 / TS / fMP4）→ 决定实现分支；
- `hls.rs`：`m3u8-rs` 解析 + 分段并发下载（复用 Phase 1 fetch）+ 拼接/转封装
  → 喂给 Phase 3 解码管线；
- TS 分支：`mpeg2ts-reader` 抽 PES → ADTS → symphonia `AdtsReader`；
- `hls-ffmpeg` feature 作逃生舱（`ffmpeg -i <m3u8> -f mp3 pipe:1`）。

验收：SoundCloud 公开曲目可播，AAC-HLS 与 MP3-HLS 各至少一例通过。

### 依赖关系

```
Phase 1 ──► Phase 2（网易云可先完整落地；SoundCloud 元数据先行）
   │           └──► Phase 2.5（歌单 + deck 队列 + 缓存 LRU；不依赖 3/4）
   └─────────────► Phase 3（对直链与服务统一生效；之后 autoplay 升级为无缝预取）
Phase 4 ──► SoundCloud 完整可用（其 set 元数据可在 Phase 2.5 先行）
```

**建议顺序 1 → 2 → 2.5 → 3 → 4**：先拿到"直链 + 网易云 + 歌单"的完整闭环，再攻坚 HLS。
（备选：先用直链做 Phase 3 的引擎行为验证。）

### 顺手要改的

- workspace symphonia 加 `mkv` + `opus` feature（一行）→ 解锁 `.webm` 直链。

---

## 6. 风险与合规备注

| 风险 | 说明 | 对策 |
|---|---|---|
| SoundCloud HLS 分段格式未定 | AAC-HLS 可能是 TS 或 fMP4，fMP4 在 symphonia 的 fragment 支持未验证 | Phase 4 先实测；`hls-ffmpeg` 逃生舱 |
| SoundCloud api-v2 非公开 | 官方声明不供第三方使用 | 灰色地带，README 注明；client_id 失效要有可读报错 |
| 网易云风控 / 区域 | `code -462`、部分曲目仅中国大陆 | 错误分类上抛 notices；cookie 文件支持 |
| SoundCloud 迁移期 | MP3-HLS 将随迁移结束而消失，AAC-HLS 是终态 | 选流优先 AAC、回退 MP3（与官方建议一致） |
| HLS 播放列表 URL 有效期 | 签名分段 URL 会过期 | 缓存前先完整下载；渐进模式下失败重解析一次 |
| 内存占用 | 整轨常驻内存（≈140 MiB/7min），非本计划引入 | 现状即如此，后续可另立"内存映射 PCM"课题 |
| 网易云歌单 `tracks` 不完整 | 未登录只回 ~10 首，`trackIds` 才是全量 | 取 `trackIds` + 分批 `song/detail`（500/批）或 `/playlist/track/all` 分页 |
| SoundCloud set 返回 stub 曲目 | `tracks` 条目缺 `permalink_url` | 检测后批量 `tracks?ids=…` 补全 |
| 队列换曲是硬切 | `Command::Load` 重建 flow，非无缝 | 第一版接受；无缝化（预热换流）留作后续优化 |
| 缓存无限增长 | 歌单场景 1000 首 ≈ 5 GB | `cache limit` LRU 淘汰（Phase 2.5） |

---

## 7. 待确认问题（默认取推荐项，均已写入对应章节）

1. **队列归属**：推荐 **per-deck 队列**（DJ 软件惯例）；备选全局队列绑焦点 deck。
2. **登录范围**：第一版**只做公开歌单（URL 直达，零登录）**；"我的歌单/日推"等登录态
   功能后置（涉及扫码/cookie 导入 + token 存储）。
3. **autoplay 时机**：Phase 2.5 先做"播完硬切下一曲"，Phase 3 之后升级为
   接近尾声即预取的无缝衔接。
4. **SoundCloud HLS**：纯 Rust 自研为主、`hls-ffmpeg` 作可选逃生舱（推荐），还是
   SoundCloud 一上来就先走 ffmpeg。
5. **落地顺序**：按 `1 → 2 → 2.5 → 3 → 4`（推荐），还是先用直链验证 Phase 3 的
   引擎行为。

合规：个人本地使用场景；网易云/SoundCloud 均为非官方接口，若分发需在 README 说明
接口性质与条款风险（yt-dlp 同类项目的通行做法）。
