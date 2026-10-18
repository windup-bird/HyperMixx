# Rekordbox 曲库格式调研报告

> 目的：为 Hypermixx 的「曲库管理 + 读取 Rekordbox 数据」做准备。本文只做调研：各格式版本、
> 读取方法、可用库与许可、以及可落地的技术路径。**不含实现代码。**
>
> 调研时间：2026-10。结论以 2025-10 起的开源规范 `v1vendi/onelibrary`（MIT）为主要依据，
> 该规范带 `[VERIFIED]` / `[REPORTED]` / `[UNKNOWN]` 标注，本文沿用这个态度：不确定的明说不确定。

---

## 0. 一句话结论

- Rekordbox 现在主推 **OneLibrary**（原 Device Library Plus，2025-10 联合发布，RB 7.2.5 正式改名），
  它是 USB/SD 导出用的 **SQLCipher 加密 SQLite（`exportLibrary.db`）**，正在取代旧的二进制
  **DeviceSQL（`export.pdb`）**——但**不删除**旧格式，RB7 两个都写。
- 本地库仍是 **`master.db`**（RB6/7，SQLCipher 加密 SQLite，`djmd*` 表）。
- **beatgrid / 波形 / cue 都不在上述任何数据库里**，它们在旁路的 **ANLZ** 文件
  （`PIONEER/USBANLZ/**/ANLZ0000.DAT/.EXT`），格式与旧 PDB 导出共用。
- **两个加密库（`master.db` 与 `exportLibrary.db`）的口令都已公开且非机器相关**，因此**无需 GPL 的
  `rbox`**，用 MIT 的 `rusqlite` + `bundled-sqlcipher` 即可读。

---

## 1. 格式与版本总览

| 格式 | 文件 | 版本 / 引入 | 加密 | 内容 | 适用平台 |
|---|---|---|---|---|---|
| **master.db** | `~/Library/Pioneer/rekordbox/master.db`（macOS）/ `%APPDATA%\Pioneer\rekordbox\master.db`（Win） | RB 6 / 7（当前 7.x） | SQLCipher4 | 完整本地收藏：曲目/播放列表/历史/cue/分析引用 | Win/macOS |
| **OneLibrary**（原 Device Library Plus） | U 盘/SD `PIONEER/rekordbox/exportLibrary.db` | RB 6.8+ 引入，7.x 扩展；2025-10 更名 | SQLCipher4 | 曲目/播放列表/MyTag/历史（cue 不在内） | 跨平台（含 Linux 读 U 盘） |
| **Device Library（legacy）** | `PIONEER/rekordbox/export.pdb` + `exportExt.pdb` | DeviceSQL | 明文二进制 | 曲目/播放列表/历史/Artwork | 跨平台 |
| **ANLZ** | `PIONEER/USBANLZ/<P0xx>/<8hex>/ANLZ0000.DAT`（+`.EXT`）/ 本地 `.2EX` | 未变 | 明文（设备导出） | beatgrid / 波形 / hot+memory cue / 短语结构 | 跨平台 |
| **Rekordbox XML** | 任意 `rekordbox.xml` | 1.0.0 | 明文 | 元数据/BPM/Key/播放列表/cue（精度最低） | 跨平台 |
| **My-Setting** | `MYSETTING.DAT` / `MYSETTING2.DAT` / `DEVSETTING.DAT` | — | 明文 | 播放器偏好（非曲库） | 跨平台 |

**关键关系**

- **OneLibrary == Device Library Plus**，完全兼容，只是改名（RB 7.2.5，2025-10-21）。
  支持硬件：CDJ-3000X、XDJ-AZ、OPUS-QUAD、OMNIS-DUO，以及 CDJ-3000 固件 3.15+。
- OneLibrary 由 AlphaTheta 与 Algoriddim 联合推动，djay Pro、Traktor 已接入；
  官方目标是把「播放列表 / cue / beatgrid」做成跨品牌通用库格式。
- **一个设备上可能同时存在两套库**（`export.pdb` 与 `exportLibrary.db`）。
  规范建议：**两者都在时优先 OneLibrary**（rekordbox 现在维护它，播放器也优先用它）。
- 转换是单向的：`Convert to OneLibrary`（RB7）把 legacy 库转成 OneLibrary；反之不会自动发生。

---

## 2. OneLibrary（`exportLibrary.db`）详解

### 2.1 加密参数

**SQLCipher 4 默认参数，无需 compatibility pragma**：

| 参数 | 值 |
|---|---|
| 加密算法 | AES-256-CBC |
| 页大小 | 4096 |
| KDF | PBKDF2-HMAC-SHA512，**256,000** 次迭代 |
| 每页 MAC | HMAC-SHA512 |

> ⚠️ **最常见的集成坑**：口令以**字符串**传入（`PRAGMA key = '...'`）。
> 写成 `PRAGMA key = "x'...'"`（hex raw key）会报 `file is not a database`。

### 2.2 口令（公开、非机器相关）

| 数据库 | 口令 | 状态 |
|---|---|---|
| `exportLibrary.db` | `r8gddnr4k847830ar6cqzbkk0el6qytmb3trbbx805jm74vez64i5o8fnrqryqls` | [VERIFIED] 打开真实 RB7 导出 |
| `master.db` | `402fd482c38817c35ffa8ffb8c7d93143b749e7d315df7a81732a1ff43608497` | [VERIFIED] 打开本地库 |

两者**不通用**（互相开会报 `file is not a database`）。口令本身报告为「与授权、机器无关」，
所有安装一致；但**在二进制里恢复口令的混淆链尚未破解**（与实现无关，因为我们直接用已公开的口令）。

> 合法性说明：这是**互操作**用途，读用户自己的库；`v1vendi/onelibrary` 为 MIT 且明确声明不规避
> 音频内容 DRM、不附带版权素材。

### 2.3 ⚠️ WAL 陷阱（读之前必须处理）

rekordbox 会把**新导出的大部分数据留在 write-ahead log**。曾观察到 118 KB 的 `exportLibrary.db`
旁边躺着一个 **1.1 MB 的 `exportLibrary.db-wal`**；单独打开主库会得到一个**几乎空的库且不报错**。

**做法**：读之前把 `exportLibrary.db` + `-wal` + `-shm` **一起复制**，并做一次
`PRAGMA wal_checkpoint(TRUNCATE)`，再打开。

### 2.4 Schema（22 张表，已完整还原）

声明类型只有 `integer` / `varchar`；**没有任何 NOT NULL / DEFAULT / FOREIGN KEY**，关系全靠命名与数据推断。

| 表 | 用途 |
|---|---|
| `content` | 曲目（主表） |
| `cue` | cue 点/hot cue/loop（**导出时通常为空**，见 2.6） |
| `artist` / `album` / `genre` / `label` / `key` / `color` / `image` | 查找表 |
| `playlist` / `playlist_content` | 播放列表树 + 有序成员 |
| `myTag` / `myTag_content` | MyTag 树 + 归属 |
| `history` / `history_content` | 播放器写入的 set 历史 |
| `hotCueBankList` / `hotCueBankList_cue` | hot cue 库 |
| `menuItem` / `category` / `sort` | 浏览界面配置 |
| `property` | 设备描述（`dbVersion` 等） |
| `recommendedLike` | 曲目相似度（通常空） |

**关键字段语义（已数据验证）**

| 字段 | 含义 |
|---|---|
| `content.bpmx100` | **centi-BPM**：`12400` = 124.00 BPM |
| `content.length` | 时长，**整秒** |
| `content.rating` | 0–5 星（**不是** master.db 的 0/51/…/255 编码） |
| `content.path` | **设备相对** POSIX 路径，如 `/Contents/Artist/Album/Track.mp3` |
| `content.masterContentId` | 源 `master.db` 中 `djmdContent.ID` |
| `content.masterDbId` | 源库标识（一次导出内恒定） |
| `content.analysisDataFilePath` | 指向 ANLZ `.DAT` 的**设备相对**路径 |
| `content.cueUpdateCount` | cue 变更计数（cue 是否存在的唯一线索） |
| `color` | 1 Pink, 2 Red, 3 Orange, 4 Yellow, 5 Green, 6 Aqua, 7 Blue, 8 Dark |
| `property.dbVersion` | RB7 导出为 `"1000"` |
| `*_content.sequenceNo` | 父节点内 1-based 排序 |

**四张树形表**（`playlist` / `hotCueBankList` / `history` / `myTag`）结构相同：
`attribute` 判别符 + 自引用 `*_id_parent` + `sequenceNo`。
（`playlist.attribute`：`0`=播放列表，`1`=文件夹，`4`=智能播放列表——值来自 master.db 的对应表。）

**必须照抄 rekordbox 的拼写错误**（写回时）：`album.isComplation`（少个 i）、
`cue.OutFileOffsetInBlock`（大写 O）。只读则不受影响。

### 2.5 OneLibrary 不含 beatgrid/波形/cue

`content` **没有** beatgrid/waveform/phrase 列；只有 `analysisDataFilePath` 指向 ANLZ。
**cue 表在导出时通常是空的**：曾有一个含 3 个 hot cue + 2 个 memory cue + 1 个 loop 的导出，
`cue` 表 **0 行**，全部 cue 都在 ANLZ 里。数据库里唯一的痕迹是 `content.cueUpdateCount`。

→ 结论：**凡是要 beatgrid/cue/波形，必须读 ANLZ**。

---

## 3. master.db（本地库）详解

### 3.1 加密

同为 **SQLCipher4**（与 OneLibrary 同族参数）。口令见 §2.2。

### 3.2 结构

- 主表 `djmdContent`（曲目），外键指向 `djmdArtist` / `djmdAlbum` / `djmdGenre` / `djmdKey` /
  `djmdLabel` / `djmdColor` 等。
- 播放列表类对象一律**拆两张表**：`djmd<NAME>`（节点/元数据） + `djmdSong<NAME>`（成员与顺序）。
  例：`djmdPlaylist` + `djmdSongPlaylist`，`djmdHistory` + `djmdSongHistory`，
  `djmdMyTag` + `djmdSongMyTag`。
- 大多数表有一组**默认列**：`UUID`、`rb_data_status`、`rb_local_data_status`、`rb_local_deleted`、
  `rb_local_synced`、`usn`、`rb_local_usn`、`created_at`、`updated_at`（同步用，diff 时要忽略）。

**关键表速查**

| 表 | 关键字段/含义 |
|---|---|
| `djmdContent` | `ID`、`FolderPath`、`FileNameL`、`Title`、`ArtistID`/`AlbumID`/`GenreID`/`KeyID`/`LabelID`/`ColorID`、`BPM`（平均 BPM，文档误标 Unit: Second）、`Length`(秒)、`Rating`、`ReleaseYear`、`FileType`(mp3=1,mp4=3,aac=4,flac=5,alac=6,wav=11,aiff=12,video=16)、`ImagePath`(相对库根)、`AnalysisDataPath`(相对库根)、`SampleRate`、`BitDepth`、`ISRC`、`DJPlayCount` |
| `djmdCue` | `ContentID`、`InMsec`/`OutMsec`(ms)、`InFrame`/`OutFrame`(**1/150 秒帧**)、`InMpegFrame`/`InMpegAbs`(VBR/ABR 用)、`Kind`(**0=memory cue，否则为 hot cue 编号**)、`Color`(-1 无)、`ActiveLoop`、`Comment` |
| `djmdPlaylist` | `Name`、`Seq`、`ParentID`、`Attribute`(**0=播放列表,1=文件夹,4=智能播放列表**)、`SmartList`(XML 条件串) |
| `djmdSongPlaylist` | `PlaylistID`、`ContentID`、`TrackNo` |
| `djmdKey` | `ScaleName`（调性名，如 `Am` / `8A`） |
| `djmdProperty` | `DBID`、`DBVersion`、`BaseDBDrive`、`CurrentDBDrive`（此表**不用**默认列） |
| `djmdMixerParam` | `GainHigh`/`GainLow`、`PeakHigh`/`PeakLow`（f32 拆成两个 u16；dB = `20*log10(f)`） |

> 时间单位**两套并存**：`InMsec`（毫秒）与 `InFrame`（1/150 秒 = 6.667 ms）。
> VBR/ABR 的 MP3 另有 `InMpegFrame`（约 1/75 秒，用于寻址）。

---

## 4. Legacy DeviceSQL（`export.pdb` / `exportExt.pdb`）详解

社区文档非常完整（henrybetts / flesniak / crate-digger），Rust 侧有 **rekordcrate**（MPL-2.0）。

### 4.1 文件布局

- 全小端；文件为固定大小页的整数倍，`export.pdb` **页 = 4096 字节**。
- **页 0 = 文件头**：`len_page`、表数量、`next unused page`、`sequence`（每次编辑 +1），
  随后是 **16 字节/个的表条目**：`row type`、`empty candidate`、`first page index`、`last page index`。
- **每个页以 0x28 字节头开始**：本页索引、row type、`next page`、编辑时 sequence、
  24-bit 位域（13 位 `num_row_offsets` + 11 位 `num_rows`）、页标志、空闲/已用字节等。
- 页内是**堆**，行零散分布；行的偏移索引**从页尾向前**以 16 字节一组构建。
- 页标志的 **bit 0x40 = 空页**（多数表链的头部），**必须跳过**，否则会把尾部字节当行索引解析出垃圾。
- **清除的 presence 位是硬停止**，不是提示：删除的行保留索引槽和偏移，rekordbox 可复用或遗弃。

### 4.2 表类型（PlainPageType）

`Tracks(0)`、`Genres(1)`、`Artists(2)`、`Albums(3)`、`Labels(4)`、`Keys(5)`、`Colors(6)`、
`PlaylistTree(7)`、`PlaylistEntries(8)`、`Unknown9(9)`、`Columns(10)`、`HistoryPlaylists(11)`、
`HistoryEntries(12)`、`Artwork(13)` 等（`exportExt.pdb` 用 `ExtPageType`，多出 ext 列与 ext 曲目）。
字符串是 **DeviceSQLString**（7-bit/8-bit 两种变体，`binrw` 实现）。

### 4.3 读 vs 写

- **只读非常简单**（顺序遍历页链即可）；**写入很麻烦**（页堆分配、原位重建行索引、行放不下要裂页），
  且收益小——rekordbox 自己能把 legacy 转成 OneLibrary。
  → **建议只读**。`v1vendi/onelibrary` 与 `rekordcrate` 都只读 legacy。

- 校验方式很优雅：**转换过的设备同时带两套库**，即同一份数据写了两遍，逐表行数/字段应一致
  （14 曲 → 14/15/12/11/10/10/8/11/1/11/28 逐表吻合；`title/tempo/duration/bitrate/rating/year/file_size/analyze_path`
  全一致）。

---

## 5. ANLZ（`.DAT` / `.EXT` / `.2EX`）详解

**beatgrid / 波形 / cue / 短语结构都在这里。** 格式与旧 PDB 导出共用，文档来自 crate-digger / pyrekordbox。

### 5.1 外层结构

- 文件头：4 字符魔术 `PMAI` + `len_header`（通常 `0x1c`）+ `len_file`。
- 之后是**带标签的段**，每段：`fourcc` + `len_header` + `len_tag`（加到段起点即下一段）。

**标签顺序（常见）**

| 文件 | 标签 |
|---|---|
| `.DAT` | `PPTH`, `PVBR`, `PQTZ`, `PWAV`, `PWV2`, `PCOB`, `PCOB` |
| `.EXT` | `PPTH`, `PCOB`, `PCOB`, `PCO2`, `PCO2`, `PQT2`, `PWV3`, `PWV4`, `PWV5`, `PVB2`, `PSSI` |
| `.2EX` | `PPTH`, `PWV6`, `PWV7`, `PWVC` |

### 5.2 我们最关心的标签

| 标签 | 含义 | 关键字段 |
|---|---|---|
| `PQTZ` | **beat 网格**（`.DAT`） | 每条 beat：`beat_number`(1–4)、`tempo`(**×100 的 BPM**)、`time`(**毫秒**) |
| `PQT2` | 扩展 beat 网格（`.EXT`） | 结构未完全公开；`entry_count` 通常等于 PQTZ 条目数 |
| `PCOB` | **cue 列表**（memory 或 hot），`type`=0/1 | 每条 `PCPT`(38 字节)：`hot`(0=memory,1–8=hot A–H)、`type`(1=点,2=loop)、`time`(ms)、`loop_time`(ms，`0xFFFFFFFF`=非 loop) |
| `PCO2` | 扩展 cue（`.EXT`），带**颜色与注释** | 含 PCOB 的全部信息 + 颜色列 |
| `PPTH` | 音频文件路径 | `len_path` + UTF-16**BE** 字符串（带尾 `\0`） |
| `PVBR` / `PVB2` | VBR 寻址（PVB2 用于 FLAC） | 帧索引表 |
| `PWAV`/`PWV2` | 单色波形预览（400 字节） | 每字节：低 5 位高度、高 3 位色深 |
| `PWV3` | 单色波形细节 | 每秒 150 条（75 帧/秒 × 半帧） |
| `PWV4` | **彩色波形预览**（1200 列，6 字节/条） | RGB+高度 |
| `PWV5` | 彩色波形细节 | 每秒 150 条 |
| `PSSI` | 短语/歌曲结构（Intro/Up/Down/Chorus/Outro…） | **RB6 导出时按 base pattern XOR 掩码** |
| `PWV6`/`PWV7`/`PWVC` | `.2EX` 波形标签 | — |

### 5.3 ⚠️ `.DAT` 与 `.EXT` 故意不一致

- `.DAT` 只带**旧播放器能懂的 hot cue 子集**；`.EXT` 带**完整现代集合**。
  实测同一曲目 `.DAT` 2 个 hot cue、`.EXT` 3 个。
- **读 cue/beatgrid 优先 `.EXT`**；写则两者都要，`.DAT` 作为兼容子集。
- 无 cue 的曲目里 `PCOB` 只有 24 字节、`PCO2` 只有 20 字节（仅包头），与空的 `cue` 表一致。

### 5.4 本地 vs 设备导出

- 设备导出的 ANLZ（`PIONEER/USBANLZ/...`）是**明文**，可直接解析（[VERIFIED]）。
- 本地 RB6 的分析文件位于 `%APPDATA%\Pioneer\rekordbox\share\PIONEER\USBANLZ`（rekordcrate 亦指向此）。
  本地波形有时以 `.2EX` 提供。**本地文件是否全部明文、与设备导出是否逐字节相同，本文标 [UNKNOWN]**，
  实现时应对本地路径单独验证。

---

## 6. Rekordbox XML（`rekordbox.xml`）详解

Pioneer 有官方 `xml_format_list.pdf`。要点：

```xml
<?xml version="1.0" encoding="UTF-8" ?>
<DJ_PLAYLISTS Version="1.0.0">
  <PRODUCT Name="rekordbox" Version="7.x" Company="AlphaTheta"/>
  <COLLECTION Entries="1234">
    <TRACK TrackID="1" Name="..." Artist="..." Album="..." Genre="..." Kind="MP3 File"
           Size="..." TotalTime="..." AverageBpm="128.00" Tonality="Am" Label="..."
           DateAdded="2025-01-01" BitRate="320" SampleRate="44100" PlayCount="0" Rating="204"
           Location="file://localhost/.../track.mp3" Colour="0xFF007F">
      <TEMPO Inizio="0.232" Bpm="128.00" Metro="4/4" Battito="1"/>
      <POSITION_MARK Name="A" Type="0" Start="12.5" End="-1" Num="0"/>
    </TRACK>
  </COLLECTION>
  <PLAYLISTS>
    <NODE Type="0" Name="ROOT" Count="11">
      <NODE Name="Favourites" Type="1" KeyType="0" Entries="56">
        <TRACK Key="1"/><TRACK Key="2"/>
      </NODE>
    </NODE>
  </PLAYLISTS>
</DJ_PLAYLISTS>
```

**要点**

- `TRACK.Location` 是 **file:// URI**，路径需百分号解码。
- **`Rating` 编码**：0/51/102/153/204/255 对应 0–5 星（与 OneLibrary 的 0–5 **不同**）。
- **多个 `TEMPO`** 构成 beatgrid（`Inizio` 秒、`Bpm`、`Metro` 拍号、`Battito` 小节内拍位）。
- **多个 `POSITION_MARK`** 为 cue：`Type` 0=Cue,1=FadeIn,2=FadeOut,3=Load,4=Loop；
  `Num` hot cue A=0,B=1…，memory cue = -1。
- `NODE`：`Type=0` 文件夹、`Type=1` 播放列表；`KeyType` 0=TrackID、1=Location.
- 颜色为 RGB 3 字节（Rose/Red/Orange/Lemon/Green/Turquoise/Blue/Violet）。
- 优点：**明文、跨平台、零 C 依赖**；缺点：**精度最低、也是最容易过时**（要用户手动导出）。

---

## 7. 读取方法与可用库

### 7.1 加密 SQLite（`master.db` / `exportLibrary.db`）

| 方案 | 语言 | 许可 | 说明 |
|---|---|---|---|
| **rusqlite + `bundled-sqlcipher`** | Rust | MIT | ✅ 推荐。自带 SQLCipher，无需系统库；`bundled-sqlcipher-vendored-openssl` 进一步免装 OpenSSL（**编译变重**）|
| sqlcipher CLI / `sqlcipher_export` 解密成明文副本再读 | 任意 | BSD | ✅ 备选，依赖用户装 sqlcipher |
| `rbox` | Rust | **GPL-3.0** | ❌ 与本项目 MIT 冲突；默认还带 vendored OpenSSL |
| `better-sqlite3-multiple-ciphers` | Node | MIT | 参考实现（`onelibrary-connect`） |
| `sqlcipher3-wheels` / SQLAlchemy | Python | MIT/BSD | 参考实现（`pyrekordbox`） |

> **本项目建议**：`rusqlite` 开 `bundled-sqlcipher`，并把「读加密库」做成 **Cargo feature**，
> 默认不编，避免拖慢常规构建。

### 7.2 Legacy PDB + ANLZ

| 库 | 语言 | 许可 | 覆盖 |
|---|---|---|---|
| **rekordcrate** | Rust | **MPL-2.0** | ✅ `pdb`(export/exportExt) + `anlz`(.DAT/.EXT/.2EX) + `setting` + `xml`；可读可写 round-trip |
| crate-digger | Java | — | PDB/ANLZ 权威逆向来源（kaitai `rekordbox_pdb.ksy`） |
| pyrekordbox | Python | MIT | master.db + ANLZ + XML + OneLibrary |
| rekordbox-parser | JS | MIT | PDB/ANLZ |
| onelibrary-connect | TS | MIT | OneLibrary 只读（`exportLibrary.db`） |
| v1vendi/onelibrary | Python + JS | MIT | **OneLibrary/PDB/ANLZ 规范 + 读写 + 浏览器查看器** |

> MPL-2.0 是**文件级** copyleft：作为依赖链接进 MIT 项目没问题；
> 只有**改动 rekordcrate 自己的文件**时那些文件要留在 MPL。**不要拷贝其源码进本仓库**。

### 7.3 XML / 扫描 / 标签

| 库 | 最新 | 许可 | 用途 |
|---|---|---|---|
| `quick-xml` | 0.42 | MIT | 解析 Rekordbox XML（**不用 GPL 的 rbox xml**） |
| `lofty` | 0.25 | MIT/Apache-2.0 | 读音频文件标签（ID3/Vorbis/MP4/FLAC…） |
| `walkdir` | 2.5 | Unlicense/MIT | 目录扫描 |
| `rusqlite` | 0.40 | MIT | 本应用自己的曲库存储 |

### 7.4 许可红绿灯

- ✅ 可依赖：`rusqlite`、`quick-xml`、`lofty`、`walkdir`、`rekordcrate`(MPL，勿改勿抄)。
- ⛔ 不要依赖：`rbox`（GPL-3.0）。
- 参考实现（MIT/BSD，可读其思路）：`pyrekordbox`、`onelibrary-connect`、`v1vendi/onelibrary`、`crate-digger`。

---

## 8. 技术路径建议（面向 Hypermixx）

> 现状：workspace 已分层 `cli → {audio, library, stems, midi, core}`，`library` 只管分析
> （`BeatSpec`/`GridCompiler`/`TrackInfo`/`Waveform`），且已有 `TrackId`/`Metadata` 注释为「未来数据库」。
> `profile` 已预留 `[preference]` 段。

### 8.1 分层与落点

- **扩展现有 `hypermixx-library`**（用户已选）：新增 `catalog`（本应用曲库存储）与
  `import/*`（Rekordbox 各格式解析、映射到 `core` 类型）。
  好处是与 `BeatSpec`/`Key`/`TrackAnalysis` 同屋，映射零摩擦。
- 备选：独立 `hypermixx-catalog`（只依赖 `core`），让 `library` 继续只做分析、构建更轻。
  （若在意 `stratum-dsp` 已让 library 偏重，可考虑；否则扩展 library 更省事。）
- 依赖全部用 **feature 门控**，默认构建保持轻量：
  `catalog`(rusqlite) · `rekordbox-xml`(quick-xml) · `scan`(walkdir+lofty)
  · `rekordbox-pdb`+`anlz`(rekordcrate) · `master-db`/`onelibrary`(rusqlite bundled-sqlcipher，**不默认**)。

### 8.2 数据源优先级（建议）

1. **OneLibrary `exportLibrary.db`** —— 现在主推、跨平台（Linux 也能读 U 盘），
   + **`USBANLZ` 的 ANLZ** 拿 beatgrid/cue/波形。
   ⚠️ 读前必须 `.db`+`-wal`+`-shm` 一起 copy + checkpoint。
2. **Legacy `export.pdb`/`exportExt.pdb`** —— 兼容旧 U 盘，用 `rekordcrate`，同样配 ANLZ。
3. **Rekordbox XML** —— 零 C 依赖的兜底入口（用户在应用里 File→Export 即可）。
4. **`master.db`** —— 顺带支持（同一套 SQLCipher 机制），但仅 Win/macOS 有本地库。

### 8.3 映射到 `core` 类型

| 来源 | 目标 |
|---|---|
| `content.bpmx100 / 100`、XML `AverageBpm` | `BeatSpec::rigid(bpm, first)` → 常数 `BeatGrid` |
| ANLZ `PQTZ` 的 `tempo`/`time`（ms） | **精确** `BeatGrid`（ms → 采样帧，按引擎率换算）——优先于常数网格 |
| `key.name` / XML `Tonality` | `Key`（传统 + Camelot） |
| ANLZ `PCOB/PCO2`（hot/memory cue、loop、颜色） | 未来 deck 的 cue/loop（`PCPT` 的 `hot`/`type`/`time`/`loop_time`） |
| ANLZ `PWV4/PWV5` | 可映射到 `Waveform`（本应用也能自算，二者可择优） |
| `playlist`/`playlist_content`、XML `NODE` | 本项目 `Playlist` 模型 |

### 8.4 本应用曲库存储

- **`rusqlite`（bundled，MIT）** 单文件 `library.db`，放 `.cache`（沿用 `HYPERMIXX_CACHE` 约定）。
- 表：`tracks`（path、title/artist/album/genre、bpm、key、duration、source、masterContentId…）、
  `playlists` / `playlist_tracks`、`cues`、`analysis`（缓存的 beatgrid/key）。
- 与运行期 `TrackInfo` 的关系：catalog 行持有**持久化**的 `TrackId` + `BeatSpec` + `Key`；
  装载时 seed 一个 `TrackInfo`，分析完成后回写。

### 8.5 风险与坑（实现时逐条处理）

1. **WAL**：不 checkpoint 会读到空库且不报错。
2. **口令以字符串传入**，不是 hex raw key。
3. **beatgrid/波形/cue 不在库里**，必须读 ANLZ；`.EXT` 优先于 `.DAT`。
4. **路径映射**：OneLibrary 存**设备相对路径**（`/Contents/...`），导入时需用户指定挂载点，
   逐条映射到本地绝对路径；`master.db` 的 `FolderPath` 反而是本机路径。
5. **`.2EX` / 本地分析文件是否明文**：设备导出已验证明文；本地路径待单独验证。
6. **`PSSI` 短语结构在 RB6 导出时被 XOR 掩码**（有公开 base pattern）。
7. **`rebokbox` 拼写错误字段**只在写回时需注意（本项目只读则无关）。
8. **SQLCipher 构建重量**：`bundled-sqlcipher-vendored-openssl` 首次编译明显变慢；用 feature 隔离。
9. **合法性/稳定性**：口令公开、非机器相关，但属未公开格式的逆向；上游 obfuscation 可能随版本漂移，
   实现应把它当作**尽力而为**（解析失败要优雅报错，而不是 panic）。
10. **不要使用 `rbox`**（GPL-3.0）；**不要拷贝 `rekordcrate` 源码**（MPL-2.0），只作为依赖。

---

## 9. 参考链接

- OneLibrary 官方：<https://alphatheta.com/en/onelibrary> ·
  <https://rekordbox.com/en/2025/10/dj-brands-unite-to-launch-onelibrary/>
- RB 7.2.5 改名说明：<https://rekordbox.com/en/2025/10/rekordbox-v725-release-information>
- OneLibrary FAQ：<https://rekordbox.com/en/support/faq/onelibrary-7>
- **OneLibrary 开源规范（MIT）**：<https://github.com/v1vendi/onelibrary>
  （`spec/ONELIBRARY.md`、`spec/DEVICESQL.md`、`spec/schema.sql`）
- OneLibrary 只读实现（TS, MIT）：<https://github.com/chrisle/onelibrary-connect>
- pyrekordbox 格式文档（MIT）：<https://pyrekordbox.readthedocs.io/en/latest/formats/db6.html>
  · `xml.html` · `anlz.html` · `onelibrary.html`
- rekordcrate（Rust, MPL-2.0）：<https://github.com/Holzhaus/rekordcrate> · <https://docs.rs/rekordcrate>
- crate-digger / 逆向（Java）：<https://github.com/Deep-Symmetry/crate-digger>
  与 <https://djl-analysis.deepsymmetry.org>
- SQLCipher API：<https://www.zetetic.net/sqlcipher/sqlcipher-api>
- Mixxx OneLibrary 支持讨论：<https://github.com/mixxxdj/mixxx/issues/15556>
