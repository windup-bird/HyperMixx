# 节拍分析算法全景：从开源实现到商业 DJ 软件

> 开源：Mixxx / OpenFadeR / stratum-dsp / timestretch-rs / HyperMixx
>
> 商业：rekordbox / Serato / Traktor / djay / VirtualDJ / Engine DJ / Mixed In Key
>
> 上半部分是一份“定性理解”文档：讲每个开源项目**怎么听音乐、怎么下判断、把信任放在哪里**，不逐行拆代码。
> 下半部分切换到商业产品，用证据分级的方式区分厂商公开事实、合理推断与未知信息，不把营销词当算法。

---

## 一、先建立框架：节拍分析其实是四件事

不管哪套实现，最终都要回答四个问题：

1. **什么时候有声音事件？**（onset / novelty）
   —— 鼓、贝斯、和弦、人声的“起音”在哪里。
2. **一分钟多少拍？**（tempo）
   —— 速度估计。
3. **拍具体落在哪？**（phase）
   —— 同样 128 BPM，拍子可以对在鼓上，也可以对在反拍上。
4. **哪一拍是一小节的第一拍？**（downbeat / meter）
   —— 4/4 的第 1 拍，还是第 2、3、4 拍。

外加第五件事：**速度变了怎么办？**（drift / tempo variation）

很多对比只盯着第 2 项（BPM 准不准），但 DJ 实际最容易出问题的是第 3 项：BPM 差 1% 还能拉 pitch 救，相位错半拍则 sync 永远对不上。

---

## 二、用比喻先记住五家的性格

| 项目 | 一句话性格 |
|---|---|
| **Mixxx** | 老练的 DJ：先听个大概，然后拿节拍器从头铺到尾，中途尽量不改速度。 |
| **OpenFadeR** | 录音师：盯着低频鼓点，先找速度，再找“鼓到底落在哪”，还会判断小节第一拍；速度变了就分段。 |
| **stratum-dsp** | 学院派：特征工程很全，速度判断很稳；但“拍具体落在哪”比较粗。 |
| **timestretch-rs** | 双人复核：一个人跟着音乐走，另一个人拿节拍器对鼓点；两人不一致就主动降低置信度。 |
| **HyperMixx** | 网格编辑器：自己不检测，拿到别人给的拍表，用统计方法压成一条稳定、可编辑、可持久化的网格。 |

---

## 三、逐家定性描述

### 1. Mixxx：成熟、保守、以“恒定速度”为默认哲学

Mixxx 把节拍分析做成了插件：默认用 Queen Mary 的实现，另一个可选插件只给 BPM、不跟踪拍。

它的听感流程大致是：

- 用一个比较传统的“频谱变化”检测起音，时间分辨率大约十几毫秒；
- 在几秒长的窗口里做自相关，再用一组“梳状滤波器”去强化周期性，并用一个偏向 120 BPM 附近的先验压住离谱答案；
- 用动态规划找一条最合理的拍序列；
- 最后，**默认把结果压成一条恒定 BPM 的网格**：找曲子里最长的稳定速度段，算出 BPM，再把相位对齐到已检测的拍上。

**强项**：稳定、便宜、工程成熟；对常规 4/4 舞曲几乎不会给出荒唐结果。

**弱项**：
- 默认不保留“每拍一个位置”的细节，而是压成恒速网格；
- 对真实变速、现场鼓手、老唱片的速度漂移不友好；
- 没有 downbeat 检测，只给拍网格；
- 时间分辨率较粗，对细微的相位偏移不敏感。

**适合**：以“能 sync、能显示 BPM”为目标的 DJ 软件默认路径。

---

### 2. OpenFadeR：音频证据优先，细节最丰富

OpenFadeR 是五家里最“从声音出发”的一套。它先把音频分成低、中、高三个频带，然后：

- 用类似 SuperFlux 的思路检测起音：只看“能量相对最近一段时间是否明显上升”，而不是简单看能量大小。这样持续的 Pad、和弦不会被误判成鼓点；
- 速度估计先用自相关提出几个候选，再用“把起音折叠到某个周期上，看有多少权重落在网格上”来评分。它不只看周期性，还看“网格是否真的解释了起音”；
- 半速/倍速问题用“用户范围 + 一个偏向 125 BPM 的宽先验”来解决；
- **相位不是用起音最强的地方，而是用 200 Hz 以下的低频包络**：底鼓起来的地方才是拍。这个选择很关键，因为 hi-hat 的起音往往和底鼓一样强，但不会让低频能量上升；
- 小节第一拍用三样证据投票：低频重音、拍与拍之间的频谱变化、音乐开始的位置。它明确只做 4/4，不猜 3/4；
- 如果检测到速度漂移，就按 8 秒窗口估计局部速度，用动态规划跟踪拍，再把拍序列切成“每段一个速度”的分段。

**强项**：相位准、抗持续音干扰、能处理变速、有 downbeat、有实测数据支撑。

**弱项**：实现复杂、参数多；只做 4/4；对非舞曲/非 4/4 材料的覆盖有限；分段和 downbeat 在真实复杂曲目上仍有提升空间。

**适合**：离线曲库分析、自动 cue、自动混音这类“值得多花 CPU 换准确度”的场景。

---

### 3. stratum-dsp：速度估计很强，拍定位偏弱

stratum 的风格是“特征工程 + 多方法融合”：

- 起音可以用能量、频谱、HFC、HPSS 等多种方法，再投票取共识；
- 速度估计是它的重点：FFT 频域 tempogram 和自相关 tempogram 双路，支持多分辨率、分频带融合、mel 特征；旧版还有自相关 + 梳状滤波作为兜底；
- 候选之间会做 2 倍/0.5 倍/1.5 倍的八度修正，并用“方法一致性”提升置信度。

也就是说，**“这首歌大概多少 BPM”是 stratum 最值得信任的部分**。

但它的“拍具体落在哪”明显弱一档：

- 文档里说用 5 状态 HMM 跟踪 ±10% 的速度变化，但按当前代码看，状态并没有真正影响起音匹配概率，实际效果更接近“从第一个起音开始，按估计 BPM 均匀铺一条网格，没有起音支撑的拍就丢掉”；
- 因此它不会真正跟随速度漂移；
- 相位锚在“第一个起音”上，而不是鼓点上；
- downbeat 是“第一拍 + 每隔一个小节”的机械规则，不做重音判断。

**强项**：BPM 估计稳、特征丰富、可调参数多、对曲库级批量分析友好。

**弱项**：拍定位和相位粗糙；变速跟踪名不副实；downbeat 不可信。

**适合**：只要 BPM 和大致网格的场景，或者作为更强后处理器的“候选提供者”。

---

### 4. timestretch-rs：双估计器，知道自己什么时候不可信

timestretch 的分析模块有两个相对独立的角色：

**角色 A：跟着音乐走的跟踪器。**
- 起音用对数压缩的频谱通量 + 能量 + 相位偏差，阈值自适应；
- 速度用 8 秒窗口的自相关 tempogram，加谐波强化（把 2 倍、3 倍周期的证据也算进来），再用 Viterbi 找一条平滑的速度曲线；
- 拍用 Ellis 风格的动态规划，在“起音显著”和“间隔接近当前周期”之间权衡；
- 之后还会把拍位置吸附到更精确的起音上。

**角色 B：拿节拍器对鼓点的刚性网格。**
- 只看 150 Hz 以下的低频起音包络；
- 在估计 BPM 附近小范围搜索，同时扫描整个相位圆；
- 用“相位决定性”判断这条刚性网格是否可信；
- 如果刚性网格和跟踪器的拍能互相印证，就采纳；如果两者互相矛盾，就明确把相位标记为“不可信”，并压低置信度。

此外，它还有 DnB 的 3:2 二次判断：当半速/倍速关系中的 1.5 倍候选明显更强时，会重新跟踪一次，避免把 174 BPM 的鼓打贝斯读成 87。

**强项**：既跟得上漂移，又能识别“量化材料”；对自己不可靠的情况有明确表达（置信度、phase_untrusted）。

**弱项**：链路长、参数多；对非电子/非量化材料的调参依赖语料；downbeat 仍以 4/4 先验为主。

**适合**：既要分析质量、又需要置信度和可解释性的场景。

---

### 5. HyperMixx：不检测，只做“稳定化 + 可编辑化”

HyperMixx 目前的实际角色不是“第六个检测器”，而是：

> **把任意后端的拍表，收敛成一条稳定、绝对帧对齐、可人工编辑、可持久化的网格。**

它的流程是：

1. 调用后端（目前主要是 stratum）拿到一串“拍的时间点”；
2. 用“相位集中度”衡量：如果这些拍真的属于同一个周期，它们折叠到一个周期上时应该聚在同一个相位；聚得越紧，说明这个周期越可信；
3. 在 90–180 BPM 的 DJ 区间里找最强的几个周期候选；
4. 用圆均值定相位，再用局部/全局最小二乘把周期和相位磨细；
5. 用内点比例、时间覆盖率、残差中位数等指标判断这条网格是否可信；不可信时标记为 degraded，而不是直接报错；
6. 最后把“一段 BPM + 起始帧”编译成绝对帧的拍网格，并做前填、尾填、接缝去重。

**强项**：

- 对“脏拍表”很稳：后端混进来的 ghost beat、半拍 onset、重复拍，会被相位集中度和内点筛选过滤掉；
- 输出是绝对帧位置，不会因为逐拍累加而漂移；
- `BeatSpec` 用“段”表达网格，天然支持“整首恒速”“逐拍变速”“混合”三种形态；
- 后端可替换：stratum 不行可以换 timestretch，网格层不用动；
- 有明确的质量标记，不假装自己总是对的。

**弱项**：

- **没有自己的音频证据**：它不知道拍表本身是否锁在反拍上，只能用“这些拍彼此是否一致”来判断；
- **默认刚性**：`analyze` 目前只输出一段恒速网格，真实变速会被抹平；
- **八度是硬折叠**：90–180 之外的速度会被强行折进来，85 BPM 的 hip-hop 会显示成 170；
- **没有 downbeat**：只有拍，没有小节；
- 依赖后端质量：stratum 的拍定位偏粗，HyperMixx 只能修“一致性”，修不了“系统性偏反拍”。

**适合**：DJ 曲库的网格存储、导入导出、人工校正、sync/loop 的运行时基础。

---

## 四、横向对比：把“信任”放在哪

| 维度 | Mixxx | OpenFadeR | stratum-dsp | timestretch-rs | HyperMixx |
|---|---|---|---|---|---|
| **最信任的东西** | 稳定速度段 + 节拍器 | 低频鼓点 + 起音证据 | 多方法融合的 BPM | 跟踪器与刚性网格互证 | 拍表自身的相位一致性 |
| **起音分辨率** | 较粗（十几毫秒） | 很细（2.5 ms，可选 1 ms） | 较粗（十几毫秒） | 较粗但有相位偏差补偿 | 继承后端 |
| **速度估计** | 稳，偏保守 | 稳，证据驱动 | 最强项 | 强，能跟曲线 | 不估计，只拟合 |
| **半速/倍速** | 先验软压 | 范围硬折 + 宽先验 | 候选合并 + 八度修正 | 宽先验 + 3:2 二次判断 | 硬折到 90–180 |
| **相位** | 动态规划 + 后处理平均 | 低频包络定拍 | 第一个起音定锚 | 跟踪 + 刚性网格双路 | 圆均值 + 最小二乘 |
| **变速** | 名义上有，默认压平 | 有，窗口 + 分段 | 名义上有，实际较弱 | 有，逐帧曲线 + 分段 | 目前没有 |
| **downbeat** | 没有 | 有，只做 4/4 | 机械规则 | 有，带置信度 | 没有 |
| **输出形态** | 恒速网格或拍表 | 每拍时间 + 小节位置 | 秒列表 + 小节 | 绝对采样点 + 分段 + 置信度 | 绝对帧 + 可编辑段 |
| **对自己不确定的表达** | 较弱 | 较弱 | 置信度 | 强（置信度 + phase_untrusted） | 强（degraded + 质量指标） |

---

## 五、几个容易误解的点

### 1. “用了 HMM/Viterbi”不等于“真的在跟速度变化”

关键不在名字，而在**状态是否真的影响观测概率**。如果所有状态对同一个起音的匹配概率一样，Viterbi 就只是在一条固定网格上做选择，速度并不会真的变。看实现时要盯住这一点，而不是看文档里的算法名。

### 2. “恒定 BPM”不是偷懒，而是 DJ 软件的默认哲学

Mixxx 默认压成恒速网格，HyperMixx 目前也只发恒速段。原因是：DJ 的 sync、loop、beat jump 都假设一条稳定的时间轴；真实录音里的微小速度漂移，很多时候宁可被“校正掉”，也不希望网格跟着抖。问题只在于：**遇到真正变速的曲子，要有能力切换成多段模式**。

### 3. 相位错误比 BPM 错误更致命

BPM 差 0.5%，拉一下 pitch 就回来了；相位错半拍，sync 会一直“对不上”，loop 会切在反拍上。所以“用低频鼓点定相位”这类看似朴素的策略，实际价值很高。

### 4. 半速/倍速是音乐语义问题，不是数学问题

同样一段起音，数学上 87 和 174 都可以解释。不同项目的回答方式不同：

- 有的用先验（偏向 120 附近）；
- 有的用用户范围（85–175，范围就是承诺）；
- 有的用“半速/倍速候选的显著度”比较；
- 有的直接硬折叠。

没有绝对正确，只有“对目标曲库是否合适”。

### 5. downbeat 和 beat 是两件事

拍网格只回答“每拍在哪”；downbeat 回答“哪拍是第 1 拍”。后者需要重音、和声变化、段落起点等额外证据。很多系统（包括 HyperMixx 目前）只做前者。

---

## 六、对 HyperMixx 的三条路线

### 路线 A：保持“稳” —— 继续做网格稳定化

适合：以 DJ 曲库为主、以 sync/loop/编辑为核心。

- 保持刚性拟合 + 质量门 + 绝对帧网格；
- 把 `degraded` 和各项质量指标暴露给 UI，让用户知道哪首需要手动修；
- 把 90–180 的硬折叠改成可配置，至少让用户能选“这首按 85 或 170 读”。

### 路线 B：补“准” —— 给拟合加音频证据

适合：想明显降低“相位锁在反拍”的概率，但不想重写检测器。

- 在已有 mono 音频上算一个低频起音包络；
- 对候选 `(BPM, 相位)` 额外打一个“低频重音得分”；
- 让这个得分与相位集中度一起决定最终相位，而不是只看拍表自身是否一致；
- 成本低，收益直接。

### 路线 C：补“全” —— 接一个真正的变速/小节检测器

适合：要处理 live、老唱片、breakdown、自动 cue、phrase 对齐。

- 最省事：接 timestretch 的分析模块，它已经输出每拍位置、分段和置信度；
- 或者参考 OpenFadeR 的动态分段与 downbeat 证据；
- `BeatSpec` 和 `GridCompiler` 已经支持多段，只差“谁来填这些段”。

**建议顺序**：先 A（把现有东西做扎实、把质量表达清楚）→ 再 B（低成本提升相位准确度）→ 最后 C（按产品需求决定是否值得做变速和 downbeat）。

---

## 七、开源五家快速记忆卡

- **Mixxx**：稳、保守、默认恒速；适合“能用”，不适合“研究”。
- **OpenFadeR**：低频鼓点定相位、证据投票定小节、窗口 + 分段跟变速；细节最丰富。
- **stratum-dsp**：BPM 强、拍定位弱；适合当“速度候选提供者”。
- **timestretch-rs**：跟踪器 + 刚性网格双估计器；最诚实地说“我可能不可信”。
- **HyperMixx**：不做检测，做“脏拍表 → 稳定网格”的收敛与编辑层；优势在数据模型和鲁棒性，短板在音频证据、变速和小节。

---

## 八、商业 DJ 软件：授权派、自研派与 2026 年的收敛

> 调研范围：rekordbox、Serato DJ、Traktor、djay Pro、VirtualDJ、Engine DJ、Mixed In Key，以及其背后的 zplane 技术授权体系。
>
> 调研日期：2026-11-07
>
> 文档性质：技术调研。商业软件核心算法闭源，本部分**不声称掌握其实现细节**，只区分三类信息：可验证的授权关系与产品行为、基于公开资料的合理推断、目前无法确认的部分。
>
> 前七节讨论开源实现的“算法性格”；本节切换到商业产品，重点不是猜源码，而是建立一张**可信的技术地图**：谁用了谁的技术、产品表现出什么能力、哪些说法有证据、哪些只是推测。

---

### 8.0 摘要

1. **商业 DJ 软件的节拍分析分为两条路线。** 一条是“授权派”，直接采用 zplane 的 AUFTAKT（节拍跟踪）、TONART（调性）、ELASTIQUE（时间拉伸），代表是 djay Pro、Mixed In Key、DJUCED/Hercules；另一条是“自研派”，核心检测闭源自研，代表是 Serato、Traktor、rekordbox、VirtualDJ、Engine DJ。
2. **目前唯一能相对具体描述的商业节拍算法是 zplane AUFTAKT V4。** 官方明确其基于深度神经网络与统计模型，支持变速、突变、拍号变化、改进的 downbeat 和多个 correction point；但网络结构、训练目标和推理流程未公开。
3. **2026 年的产品共识是“变速网格成为默认能力”。** Serato Lucid Beatgrids 默认 Flexible、VirtualDJ Fluid BPM、Traktor Pro 4 Flexible Beatgrids、rekordbox Dynamic、djay Fluid Beatgrid、Engine DJ 多 anchor，均指向同一类数据模型：以时间锚点与分段 BPM 描述整首曲子的 tempo map。
4. **downbeat 是独立于 BPM/beat 的第二阶段问题。** rekordbox 的 High Precision 主要提升 downbeat 与网格精度；Serato 将“显著改进 downbeat”作为 Lucid 的核心卖点；AUFTAKT V4 也单独强调 downbeat。这说明“拍在哪”和“哪一拍是小节第一拍”在工程上是两个任务。
5. **“AI”标签需要拆开看。** djay 的 AI 节拍来自授权 AUFTAKT（真 DNN）；VirtualDJ 以 AI 描述其 Fluid 分析；Serato 只说“重建引擎”，未确认神经网络；rekordbox 7 的 AI 主要用于 vocal 位置检测，不是 beatgrid。
6. **专利不能直接证明当前产品实现。** Serato 的 `US6266003B1` 常被误传为节拍检测专利，实际是时标/音高修改（time-scale / pitch modification）的信号处理专利；Magix 的 `US6518492B2` 是多算法 BPM 候选 + 用户 tap 修正；Pioneer Electronic Corporation 的 `US5614687A` 是 1997 年的频段电平触发式 beat 数检测。它们只能说明技术方向，不能说明现代产品仍在沿用。
7. **对 HyperMixx 的直接参照是数据模型，而不是某个神秘算法。** 商业产品的共同点是“分段 tempo map + 锚点编辑 + downbeat + 置信度/手动修正”。HyperMixx 的 `BeatSpec` 与 `GridCompiler` 已经具备承载这种模型的骨架，缺口主要在自动分段、downbeat 和音频证据。

---

### 8.1 研究范围与方法

#### 8.1.1 研究对象

本部分覆盖当前主流商业 DJ 软件中的节拍分析相关能力：

- **rekordbox**（AlphaTheta / Pioneer DJ）
- **Serato DJ Pro**（Serato）
- **Traktor Pro**（Native Instruments / inMusic）
- **djay Pro**（Algoriddim）
- **VirtualDJ**（Atomix）
- **Engine DJ**（inMusic / Denon DJ）
- **Mixed In Key**（独立分析工具）
- 以及上述产品共同涉及的 **zplane** SDK 体系

不覆盖：硬件 CDJ/XDJ 的实时 BPM 检测、DVS 时间码跟踪、Stems 分离、自动混音算法。

#### 8.1.2 证据分级

本部分在关键结论后标注证据等级：

| 等级 | 含义 | 典型来源 |
|---|---|---|
| **A** | 厂商或当事人明确陈述；官方 SDK 文档；官方 changelog / 用户手册 | zplane 官网、Algoriddim CEO 采访、VirtualDJ changelog |
| **B** | 官方产品页、官方新闻稿、官方手册中的第三方 credits | Serato 5.0 发布页、Native Instruments 支持文档、Engine DJ 用户指南 |
| **C** | 高质量第三方评测、经销商/教程、社区共识 | Lexicon、Digital DJ Tips、DJ LIFE、DJ TechTools |
| **D** | 基于产品行为的合理推断；无直接证据 | “某产品大概率自研” |

#### 8.1.3 方法论限制

- **闭源**：没有任何厂商公开完整的 onset、tempo、beat tracking、downbeat 流程。
- **营销词与算法名混淆**：Fluid、Flexible、Dynamic、Lucid 都是产品名，不是算法名。
- **专利滞后**：专利通常比产品早 5–15 年，不能反推当前实现。
- **版本差异**：同一产品不同大版本可能更换引擎，本部分以 2026 年 11 月可查资料为准。
- **无法复现**：没有厂商提供可复现的实验配置、测试集或精度指标。

因此，本部分的目标不是“还原源码”，而是建立一张**可信的技术地图**：谁用了谁的技术、产品表现出什么能力、哪些说法有证据、哪些只是推测。

---

### 8.2 产业格局：授权派与自研派

#### 8.2.1 两条路线

| 路线 | 代表产品 | 核心特征 | 优势 | 代价 |
|---|---|---|---|---|
| **授权派** | djay Pro、Mixed In Key、DJUCED、Hercules | 直接集成 zplane SDK | 上市快、质量稳定、维护成本低 | 差异化受制于供应商；授权费；无法深度定制 |
| **自研派** | Serato、Traktor、rekordbox、VirtualDJ、Engine DJ | 核心检测自研，外围可授权 | 可针对自身曲库/硬件优化；形成技术壁垒 | 研发周期长；需要持续调参和语料验证 |

#### 8.2.2 为什么会出现两条路线

节拍分析不是单点算法，而是一条链：

```
onset / novelty → tempo 估计 → beat tracking → downbeat → tempo map / grid
```

其中任何一环出错，都会在 sync、loop、beat jump、量化 cue 上放大。自研意味着要长期维护整条链，并针对自家用户曲库（EDM、hip-hop、disco、live、拉丁等）调参；授权则把这部分风险转移给 SDK 供应商。

zplane 的定位不是 DJ 软件厂商，而是**音频算法 SDK 供应商**。它的合作伙伴列表同时包含 Ableton、Akai、Algoriddim、Native Instruments、Mixed-in-Key、Mixvibes、Numark、Hercules、inMusic 等 [S2]。这意味着：

> 看到某厂商在 zplane 合作伙伴列表里，**不能直接推断它用了 AUFTAKT**；它可能只用 ELASTIQUE、TONART、STEMS 或 BARBEATQ。

这也是下文逐家分析时必须区分“合作”与“具体授权模块”的原因。

#### 8.2.3 术语澄清

| 术语 | 含义 | 常见产品名 |
|---|---|---|
| **BPM / tempo 估计** | 估计全曲或局部的速度 | BPM、Tempo |
| **beat tracking** | 在时间轴上定位每一拍 | Beatgrid、Beat Grid |
| **downbeat** | 定位小节第一拍 | Downbeat、Bar Start、Grid Start |
| **tempo map** | 用多个时间锚点描述变速 | Dynamic、Flexible、Fluid、Lucid |
| **beatgrid** | 供 sync/loop 使用的拍位置集合 | Beatgrid、CBG（Computer Beat Grid） |

商业宣传中的 Fluid / Flexible / Dynamic / Lucid 都属于**产品能力名**，不等于某种特定算法。

---

### 8.3 厂商逐一分析

#### 8.3.1 djay Pro（Algoriddim）

##### 已证实事实

- djay Pro 5（2023-12）引入 **Fluid Beatgrid**，官方描述为自动检测并跟随歌曲中的 tempo 变化与节奏变化 [S4]。
- Algoriddim CEO Karim Morsy 在 zplane 官方采访中明确表示：

  > “One of the first things we licensed was **AUFTAKT for beat-tracking** and **TONART for key detection**. We looked at different solutions for that at first, but zplane's products simply delivered better results.” [S1]

- Algoriddim 官网页脚列明使用 zplane 的 `élastique Pro V3`、`Metering V2`、`S.R.C. V1`、`fx::pack V1` [S5]。
- Algoriddim 的专利清单集中在音频分解、Neural Mix、实时 stem 处理、传输控制、音效与 pitch shifting，**没有 beat tracking 专利** [S5]。

##### 技术推断

**证据等级：B**

Fluid Beatgrid 很可能是 AUFTAKT 的产品化封装，而不是 Algoriddim 自研的独立 beat tracker。理由：

1. CEO 明确确认授权 AUFTAKT 用于 beat tracking；
2. AUFTAKT V4 的能力（变速、突变、拍号变化、改进 downbeat、多个 correction point）与 Fluid Beatgrid 的宣传高度重合；
3. 时间线吻合：AUFTAKT V4 于 2021-08 发布、V4.1 于 2022-07 发布，均早于 djay Pro 5 的 2023-12 [S1][S4]；
4. Algoriddim 的专利组合不覆盖 beat tracking，说明这条能力更可能来自外部授权。

但“Fluid Beatgrid = AUFTAKT V4”这一等式**未获官方逐字确认**。官方只确认了授权关系，没有确认版本与封装方式。

##### 尚不能确认

- Fluid Beatgrid 的后处理、锚点合并策略、与 élastique 的联动；
- 是否在 AUFTAKT 之上叠加了自研的网格平滑或 UI 逻辑。

---

#### 8.3.2 Mixed In Key

##### 已证实事实

- Mixed In Key 的 **key detection 是自研且有专利**。创始人 Yakov Vorobyev 表示，1.0 曾以 zplane 的 key 检测为基础，**2.0 起改用自研 key 检测** [S6]；Mixed In Key 官方集成页也写明 “We have our own, patented algorithm” [S7]。
- 但 **BPM 检测长期采用 zplane AUFTAKT**：
  - 2026-08 的 zplane 采访中，创始人明确说：“Our go-to suggestions are **ELASTIQUE for time-stretching and AUFTAKT for BPM detection**.” [S6]
  - 2010 年的 iPhone 应用 DJ Tempo 官方说明也写明使用 zplane 的 aufTAKT BPM 检测算法 [S24]。
- 创始人同一采访中确认：“our products still use zplane algorithms today”，并特别提到 ELASTIQUE 与 AUFTAKT [S6]。

##### 技术推断

**证据等级：A（BPM 授权）/ A（key 自研）**

> **Mixed In Key 的“调性准”和“BPM/网格”不是同一套算法。**

- key = 自研专利算法；
- BPM/beatgrid = zplane AUFTAKT。

这是商业 DJ 工具中最容易被误解的一点。把 MIK 的 key 算法当成它所有分析能力的来源，会得出错误结论。

##### 尚不能确认

- MIK 11 是否在所有平台、所有版本都使用同一版 AUFTAKT；
- beatgrid 是 AUFTAKT 原生输出，还是 MIK 做了二次处理；
- downbeat 是否由 MIK 自己补充。

---

#### 8.3.3 VirtualDJ（Atomix）

##### 已证实事实

- VirtualDJ 2026 Part 2（build 9246，2026-03-31）加入 **Fluid Beatgrids** 与 **BPM Stabilizer**。官方 changelog 原文：

  > “FLUID BEATGRIDS: AI-powered Fluid Beatgrid that handles unsteady or variable BPM tracks, from old rock to disco to warped vinyls or megamixes.” [S8]

- BPM Editor 提供两种网格模式 [S9]：
  - **Rigid**：单个 beatgrid anchor，恒定 BPM；
  - **Fluid**：多个 anchor，每个 anchor 显示自己的 BPM，用于跟随速度变化。
- Fluid 模式支持：
  - 自动提供 detection range，也可手动调整；
  - 手动增删 anchor；
  - 强制 downbeat 为 1:4 或 1:16；
  - 用 metronome 校验网格 [S9]。
- BPM Stabilizer 的定位是：把变速轨“稳定”成可手动 beatmix 的版本，而不是替代 sync [S8]。

##### 技术推断

**证据等级：C**

- VirtualDJ 的节拍检测大概率是自研，没有公开证据表明其使用 zplane AUFTAKT。
- **FAME** 在 VirtualDJ 旧版功能列表中与 “one-click beat matching and synchronisation” 并列出现 [S27]，通常被理解为**同步/匹配引擎**，而不是 BPM 检测算法。这一点属于社区解读，不是官方定义。
- Fluid Beatgrid 的数据模型是“多 anchor + 每 anchor 一个 BPM”，与 Serato Flexible、Traktor Flexible、rekordbox Dynamic 属于同一类。

##### 尚不能确认

- Fluid 分析是否使用神经网络；
- downbeat 的证据来源（低频重音、段落边界、还是纯相位规则）；
- detection range 与 BPM 半速/倍速判定的具体关系。

---

#### 8.3.4 Serato DJ Pro

##### 已证实事实

- Serato DJ 5.0（2026-10-20 正式发布）引入 **Lucid Beatgrids**，官方称其为“completely rebuilt analysis engine”，首次支持 **automatic tempo tracking**，并显著改进 downbeat detection [S10]。
- 分析模式 [S10][S11]：
  - **Flexible Beatgrid Analysis**：新的默认模式，在 tempo 变化处放置 **Beat Warp Marker**，让网格跟随整首曲子；
  - **Fixed Beatgrid Analysis**：保留，同样改进了 downbeat detection。
- BPM 变化可以按 **bar** 或 **beat** 分辨率放置；默认按 bar，以换取更平滑的网格 [S10][S11]。
- 新增编辑能力：**Set Grid Start**（重设第一个 downbeat，同时保留已有网格）、**Set Beat**（在不破坏其余网格的前提下调整小节内单个拍） [S10]。
- 官方称新算法能处理 “tempo changes, dynamic tracks, live drumming, and complex intros and outros” [S12]。
- Serato **不在** zplane 的合作伙伴列表中 [S2]。

##### 技术推断

**证据等级：C（自研）/ B（功能）**

- Lucid 大概率是 Serato 自研引擎，而非授权 AUFTAKT。依据是：无授权证据、Serato 不在 zplane 客户列表、官方措辞强调“rebuilt engine”而非“licensed technology”。
- 从 Beat Warp Marker 与 bar/beat 分辨率看，Lucid 的数据模型是**分段变速网格**，与 Traktor Flexible、VirtualDJ Fluid 同类。
- 是否使用机器学习**无法确认**。官方没有使用“neural”“AI”“ML”等词，只说“new algorithm”“rebuilt engine”。

##### 常见误传

`US6266003B1` 常被说成“Serato 的节拍检测专利”。实际上该专利的标题与摘要指向**数字信号编码、操作与 time-scale modification**，不是 beat tracking [S19]。不要用它证明 Serato 的节拍算法。

---

#### 8.3.5 Traktor（Native Instruments）

##### 已证实事实

- Traktor 的自动分析默认 BPM 范围是 **78–155 BPM**。选择 “All” 时，Traktor 会分析曲目并设置 Beatgrid，在曲目开头放置一个 Gridmarker [S13]。
- Traktor Pro 4 引入 **Flexible Beatgrids**，官方定位是“在变速曲目或跨 genre 混音时保持网格可用” [S14]。
- 官方手册对网格模型的描述是明确的：

  > “Beatgrids are defined by one or more Grid Markers, **each one setting the tempo until the following Grid Marker**. This allows tracks to have multiple tempos.” [S13]

  网格面板的 BPM 控件也以 “current tempo zone” 表述，说明每个 marker 到下一个 marker 之间是一个独立 tempo 区段。
- 自动分析与手动编辑是两件事：`Analyze` 默认只放**一个恒速 Gridmarker**，覆盖全曲；Flexible Beatgrid 的多 marker 主要靠用户在有变速的位置逐个放置 [S13][S14]。
- 手动修正时，后续 marker 会按前一个 marker 与 BPM 保持间距 [S13]。
- 时间拉伸使用 zplane 的 **élastique** 系列；Native Instruments 是 zplane 的合作伙伴 [S2]，Traktor 使用 Elastique 在业界有广泛记录 [S22]。

##### 技术推断

**证据等级：C**

- Traktor 的 BPM/beat 检测大概率是 NI 自研。没有公开证据表明其使用 AUFTAKT；zplane 合作更可能集中在 élastique。
- Flexible Beatgrid 的“多个 marker”更接近**手动/半自动分段**：社区经验是对 live drums 每隔 4 或 8 拍放置 marker，而不是像 Serato Lucid 那样默认全自动跟踪 [S25]。
- 这与 Traktor 长期的产品哲学一致：提供强大的手动网格工具，而不是完全依赖自动分析。

##### 尚不能确认

- Traktor 是否在分析阶段使用 DNN；
- 自动分析是否会在检测到明显变速时主动插入多个 marker，还是始终只给单恒速网格后由用户补；
- downbeat 是否由独立分类器产生。

---

#### 8.3.6 rekordbox（AlphaTheta / Pioneer DJ）

##### 已证实事实

- rekordbox 提供三种分析模式 [S15]：
  - **Static**：单 BPM marker，恒定网格，适合固定速度的 EDM；
  - **Dynamic**：在曲目中放置多个 beat marker，检测到 tempo shift 时调整，适合 live、funk、disco、rock；
  - **High Precision**：Static / Dynamic 均可开启，主要提升 beatgrid 与 **downbeat** 的精度。
- High Precision 的代价：Static 约增加 90% 分析时间，Dynamic 最多增加 250% [S15]。
- 支持 **BPM Range**：强制分析结果落在指定范围内，用于解决 DnB 等半速/倍速误判 [S15]。
- 支持 **Cloud Analysis**：生成音频指纹，查询 Pioneer 云端数据库；如果已有他人分析结果，直接下载 beatgrid [S15]。
- rekordbox 7 的 AI 能力主要用于 vocal 位置检测与相关工作流，不是 beatgrid [S21]。

##### 技术推断

**证据等级：C**

- rekordbox 的 beatgrid 检测大概率是 AlphaTheta/Pioneer 自研；没有找到 zplane AUFTAKT 的授权证据。
- “High Precision 主要提升 downbeat”说明：**downbeat 是独立于 BPM/beat 的第二阶段检测**，而且是最耗时的部分。这与开源实现中“beat 容易、downbeat 难”的经验一致。
- Dynamic 模式的数据模型是“多个 beat marker + 每段 BPM”，与 Serato Flexible、Traktor Flexible、VDJ Fluid 同类。
- Cloud Analysis 是工程差异点，不是算法差异点：它解释了大量“为什么 rekordbox 对流行曲特别准”的体感——有时不是算得准，而是**别人已经算过**。

##### 专利注意

Pioneer Electronic Corporation 的早期专利 `US5614687A`（1997-03-25 授权，发明人 Yoichi Yamada 等）描述了一种 beat 数检测装置：监测输入音频中某个预设频段的电平，用电平再次越过阈值的时间间隔换算 BPM [S20]。但**不能据此推断现代 rekordbox 仍在使用这套方法**。专利只能说明“曾经的方向”，不能说明“当前实现”。

---

#### 8.3.7 Engine DJ（inMusic / Denon DJ）

##### 已证实事实

- Engine DJ 用户指南的第三方 credits 明确写：
  - `ELASTIQUE efficient V3 by zplane.development`
  - `TONART V3 by zplane.development` [S16]
- 也就是说，Engine DJ 的**时间拉伸与调性检测**来自 zplane。
- 它的 beatgrid 支持 anchor、Beat Grid Lock、手动调整 BPM/锚点 [S16]。

##### 技术推断

**证据等级：D**

- Engine DJ 的 beatgrid 检测大概率是 inMusic 自研；官方 credits 中没有 AUFTAKT。若使用了 AUFTAKT，按行业惯例通常会在 credits 中列出。
- 社区对 Engine 的 beatgrid 评价分化：4/4 电子乐成功率较高，但 live / 变速 / 复杂 intro 容易漂，需要手动加 anchor [S26]。这与“自研刚性网格 + 手动锚点”的路线一致。

##### 尚不能确认

- 是否使用机器学习；
- 动态网格是自动生成还是主要靠用户 anchor；
- downbeat 的检测策略。

---

#### 8.3.8 其他值得注意的厂商

##### DJUCED / Hercules

DJUCED 用户手册明确列出其使用的 zplane SDK：

> “elastique efficient” time stretching、`TONART` key detection、`BARBEATQ` song segmentation、`[aufTAKT]` tempo and beat tracking、`z.reverb`、`Metering`、`fx::pack`、`STEMS PRO` [S17]

这是“授权派”的完整菜单：**节拍跟踪 + 段落检测 + 调性 + 时间拉伸 + 混响 + 响度 + 效果 + stem 分离**一整套采购。Hercules 的 Technical Software Manager 也在 zplane 采访中确认使用 AUFTAKT [S1]。

##### DJ.Studio

DJ.Studio 的 “Smart Beatgrid” 宣称同时处理固定速度与自然漂移的曲目，并提供 Elastique Pro 作为可选时间拉伸引擎 [S23]。其底层分析细节未公开，本部分不将其作为主要证据来源。

##### Lexicon

Lexicon 是曲库管理与格式转换工具，不是 DJ 播放软件；它的价值在于对 rekordbox / Serato / Traktor / VDJ / Engine / djay 网格格式的交叉转换与实测总结 [S15]。本部分用它作为 rekordbox 分析模式的二手来源，而非算法证据。

---

### 8.4 zplane AUFTAKT 专题

#### 8.4.1 能力清单

AUFTAKT 是目前唯一能相对具体描述的商业节拍跟踪 SDK。官方页面给出的 V4 能力包括 [S1]：

- 自动 BPM 检测；
- beat 位置检测并以 beatgrid 表示；
- 跟踪 tempo variations 与 tempo changes；
- 跟踪 **abrupt tempo changes**；
- 跟踪 **time signature** 与 time signature changes；
- 改进的 **downbeat detection**；
- 支持多个 **correction points**；
- 与 BARBEATQ 自动 cue/段落检测集成，或接受外部固定 beatgrid 输入。

时间线上，AUFTAKT V4 于 2021-08 发布，V4.1 于 2022-07 发布 [S1]。这意味着 2023 年之后发布的产品（如 djay Pro 5、VirtualDJ 2026、Serato DJ 5.0）在技术上都有可能采用 V4 代能力，但“可能”不等于“已采用”。

#### 8.4.2 技术路线

官方明确说明：

> “The most recent version of AUFTAKT, Version 4, relies on **deep neural networks and state-of-the-art statistical models** to take beat tracking to a new level of accuracy and reliability.” [S1]

训练数据覆盖：

- Western Pop、Classical、Electronic、世界音乐；
- 单乐器 loop，包括 drums、guitars、pianos [S1]。

官方声称 V4 相对旧版在所有 genre 上检测率提升 30%+，复杂 Jazz 提升 40%+ [S1]。

#### 8.4.3 不能说的部分

以下信息**没有公开**，不应在技术文档中编造：

- 网络结构（CNN / CRNN / Transformer / TCN）；
- 输入特征（mel、CQT、log-spectrogram、多带）；
- 训练目标（beat activation、tempo classification、联合损失）；
- 推理时的 DP/Viterbi 后处理；
- downbeat 是独立模型还是多任务输出；
- correction point 的合并与平滑策略。

因此，AUFTAKT 可以被描述为“DNN + 统计模型 + 多 correction point 的商业 beat tracker”，但不能被描述为“某种已知论文的实现”。

#### 8.4.4 BARBEATQ 的协同

BARBEATQ 是自动 cue / section 检测 SDK。官方说明其两种模式 [S3]：

- **有 beat 信息**：更精确，因为节奏与结构相互关联；beatgrid 不是简单地把结果 snap 到最近的拍，而是参与结构变化点的判断；
- **无 beat 信息**：独立工作。

这解释了为什么授权派产品往往“网格 + 段落 + key + 时间拉伸”是一整套采购：它们共享同一套时间轴与音频特征。

---

### 8.5 功能对比矩阵

| 产品 | BPM/beat 检测来源 | 变速网格 | downbeat | 编辑模型 | 时间拉伸 | 云分析 | 证据等级 |
|---|---|---|---|---|---|---|---|
| **djay Pro** | zplane AUFTAKT | Fluid Beatgrid | AUFTAKT V4 声称改进 | 自动 + 手动 | zplane élastique Pro V3 | 无公开 | A（授权）/ B（版本对应） |
| **Mixed In Key** | zplane AUFTAKT | 随 AUFTAKT | 未单独宣传 | 有限 | zplane élastique | 无 | A |
| **VirtualDJ** | 自研（推断） | Fluid BPM / 多 anchor | 1:4 / 1:16 强制 | 多 anchor 编辑 | 自研（未确认） | 无 | C |
| **Serato** | 自研（Lucid 重建引擎） | Flexible 默认 | 官方称显著改进 | Set Grid Start / Set Beat | 自研 | 无 | B（功能）/ C（来源） |
| **Traktor** | 自研（推断） | Flexible Beatgrids | Gridmarker | 多 marker 手动 | zplane élastique | 无 | B（功能）/ C（来源） |
| **rekordbox** | 自研（推断） | Dynamic（多 marker） | High Precision 重点 | 多 marker 编辑 | 自研 | 音频指纹复用 | B（功能）/ C（来源） |
| **Engine DJ** | 自研（推断） | 多 anchor | 未单独宣传 | anchor / lock | zplane élastique | 无 | B（credits）/ D（检测来源） |
| **DJUCED** | zplane AUFTAKT + BARBEATQ | 随 AUFTAKT | BARBEATQ 分段 | 有限 | zplane élastique | 无 | A |

> 说明：表中的“自研（推断）”表示没有公开授权证据，且产品表述更符合自研路线；不等于已证实。

---

### 8.6 专利核查

#### 8.6.1 常被引用的专利

| 专利 | 权利人 | 内容 | 与当前产品的关系 |
|---|---|---|---|
| `US6266003B1` | 原始权利人 Serato Audio Research Ltd，后更名为 Sigma Audio Research Ltd | **Method and apparatus for signal processing for time-scale and/or pitch modification of audio signals**；相位声码器式的时标/音高修改 | **不是 beat tracking**；不能证明 Serato 的节拍算法 [S19] |
| `US6518492B2` | Magix（后续转让） | 用短窗/长窗两套算法产生多个 BPM 候选，可选用户 tap 修正，并用 auto-tap 细化 | 反映 2001 年前后的 BPM 估计思路；不能证明现代 DJ 软件沿用 [S18] |
| `US5614687A` | Pioneer Electronic Corporation | **Apparatus for detecting the number of beats**；对输入音频的特定频段做电平触发与周期测量，再换算 BPM | 1997-03-25 授权；只能说明 Pioneer 早期方向，与现代 rekordbox 实现无直接对应证据 [S20] |
| Algoriddim 专利族 | Algoriddim | 音频分解、Neural Mix、实时 stem、传输控制、pitch shifting | **不含 beat tracking**；侧面支持“节拍能力来自授权” [S5] |

#### 8.6.2 为什么专利不能直接证明实现

1. **时间差**：专利从申请到授权再到产品化，可能跨越 5–15 年。
2. **范围差**：专利保护的是一个权利要求的实现，不代表产品只用这一种方法。
3. **替代差**：产品可能已经换成 DNN 或混合方法，但旧专利仍在有效期内。
4. **诉讼防御**：大厂专利组合中有大量“防御性专利”，未必对应在售功能。

因此，本部分只把专利当作“技术方向的旁证”，不作为“当前算法”的证据。

---

### 8.7 2026 年技术趋势

#### 8.7.1 变速网格从“高级功能”变成“默认能力”

时间线：

- 2023-12：djay Pro 5 引入 Fluid Beatgrid；
- 2024：Traktor Pro 4 引入 Flexible Beatgrids；
- 2026-03：VirtualDJ 2026 Part 2 引入 Fluid Beatgrids；
- 2026-10：Serato DJ 5.0 将 Flexible 设为默认分析模式；
- rekordbox 的 Dynamic 模式更早存在，并在 High Precision 中继续强化。

结论：**单 BPM 刚性网格正在退化为“固定速度专用模式”**，而不是默认。商业产品假设用户曲库中包含 live、disco、funk、老唱片、megamix 等变速材料。

#### 8.7.2 数据模型收敛：tempo map + anchor

尽管算法不同，产品层的数据模型高度一致：

```
时间锚点 1: BPM 1
时间锚点 2: BPM 2
...
时间锚点 n: BPM n
```

不同产品的叫法：

| 产品 | 锚点名 | 分辨率 |
|---|---|---|
| Serato | Beat Warp Marker | bar / beat |
| Traktor | Gridmarker | marker 间距 |
| VirtualDJ | Fluid Anchor | anchor + BPM |
| rekordbox | Beat Marker | 段 |
| Engine DJ | Anchor | anchor + BPM |

这给 HyperMixx 的直接启示是：`BeatSpec` 的“段 + BPM + 起始帧”模型与商业产品同构，不需要为了“像商业软件”而改成逐拍列表。

#### 8.7.3 downbeat 成为独立竞争点

- rekordbox：High Precision 专门提升 downbeat，代价是 +90% ~ +250% 分析时间；
- Serato：Lucid 把“显著改进 downbeat”作为核心卖点；
- AUFTAKT V4：单独强调 improved downbeat；
- Traktor / VDJ / Engine：提供 1:4、1:16、anchor 等手动修正工具。

这说明 downbeat 不是“BPM 的附属品”，而是需要额外证据（低频重音、频谱变化、段落起点、和声变化）的独立阶段。

#### 8.7.4 DNN 进入生产，但“AI”标签不等于 DNN

- **确认使用 DNN**：zplane AUFTAKT V4（官方明确）。
- **官方使用 AI 措辞**：VirtualDJ Fluid Beatgrid（未说明模型）。
- **只说“重建引擎”**：Serato Lucid（未确认 ML）。
- **AI 用于其他任务**：rekordbox 7 的 vocal 检测（不是 beatgrid）。

因此，技术选型时不能把“AI”当作算法证据；要区分“DNN beat tracking”“规则/DP 后处理”“产品营销用语”。

#### 8.7.5 云分析：rekordbox 的工程差异点

rekordbox Cloud Analysis 用音频指纹查询云端已分析结果，直接复用 beatgrid [S15]。这不是算法优势，而是**数据网络效应**：

- 流行曲被大量用户分析过 → 命中率高 → 体感“特别准”；
- 冷门曲仍然依赖本地分析。

其他厂商没有公开等价的云端网格复用能力。

#### 8.7.6 时间拉伸与节拍检测解耦

商业产品普遍把“节拍分析”和“时间拉伸”当作两个独立模块：

- djay：AUFTAKT + élastique；
- Mixed In Key：AUFTAKT + élastique；
- Traktor：自研检测 + élastique；
- Engine DJ：自研检测 + élastique；
- DJUCED：AUFTAKT + élastique。

这说明 beatgrid 的准确度与音质算法是两条独立的技术栈，不能因为某产品时间拉伸好听就推断其节拍检测也强。

---

### 8.8 对 HyperMixx 的参照意义

#### 8.8.1 商业产品的共同点

抛开算法差异，商业产品在**产品层**有三个共同点：

1. **分段 tempo map**：允许一首曲子里有多个 BPM 段；
2. **锚点编辑**：用户可以在任意位置修正网格，而不破坏其余部分；
3. **置信度/手动修正**：自动分析不追求 100%，而是提供快速发现和修正错误的工具。

#### 8.8.2 HyperMixx 现状对照

| 能力 | 商业产品 | HyperMixx 现状 | 差距 |
|---|---|---|---|
| 分段 tempo map | 普遍支持 | `BeatSpec` 支持多段，但 `analyze` 只输出单段 rigid | 缺自动分段器 |
| 锚点编辑 | 普遍支持 | `BeatSpec`/`GridCompiler` 可承载 | 缺编辑器与 UI |
| downbeat | 普遍支持 | `TrackAnalysis` 无 bar 信息 | 缺 downbeat 阶段 |
| 置信度 | 普遍有 | `FitQuality` / `degraded` | 已具备，需暴露 |
| 八度策略 | 可配置 BPM Range | 硬折叠 90–180 | 需可配置 |
| 音频证据 | 低频 kick / 重音 | `fit_rigid` 只看拍表相位集中度 | 需补低频证据 |
| 云分析 | rekordbox 有 | 无 | 非核心，可后置 |

#### 8.8.3 建议的演进路径

**阶段一：把现有刚性网格做扎实**

- 暴露 `FitQuality` 与 `degraded`，让用户知道哪些曲目需要手动修；
- 把 90–180 的硬折叠改为可配置，至少允许用户选择 85/170 的读取方式；
- 保持 `BeatSpec → GridCompiler → BeatGrid` 的绝对帧模型，不引入逐拍累加漂移。

**阶段二：补音频证据**

- 在已有 mono 音频上计算低频起音包络；
- 对候选 `(BPM, phase)` 增加“低频重音得分”，与相位集中度共同决定相位；
- 这能修正“拍表自身一致但锁在反拍”的问题，成本远低于重写检测器。

**阶段三：补变速与 downbeat**

- 用 `BeatSpec` 承载多段 tempo map；
- 接入或实现分段器（timestretch 的 `TempoSegment`、OpenFadeR 的 `segments()` 都可参考）；
- 增加 downbeat 阶段（4/4 先验 + 低频重音 + 频谱变化 + 段落起点）；
- 在 `TrackAnalysis` 中增加 bar 信息，为 phrase/loop 对齐做准备。

#### 8.8.4 不建议照搬的部分

- **不要照搬“营销词驱动”的命名**：Fluid/Flexible/Dynamic 是产品名，不是技术方案。
- **不要假设商业软件都用了 DNN**：只有 AUFTAKT V4 有明确证据。
- **不要用专利反推实现**：专利与产品之间隔着多年工程迭代。
- **不要为了“像 rekordbox”而做云端指纹**：那是数据网络效应，不是算法能力，且涉及隐私与基础设施。

---

### 8.9 结论

商业 DJ 软件的节拍分析呈现“两条路线、一个收敛”的格局：

- **两条路线**：授权派（zplane AUFTAKT）与自研派（Serato、Traktor、rekordbox、VirtualDJ、Engine DJ）。
- **一个收敛**：无论哪条路线，2026 年的产品都在向“分段 tempo map + 锚点编辑 + downbeat + 手动修正”收敛。

对 HyperMixx 而言，真正值得对标的是**数据模型与产品工作流**，而不是某个闭源算法：

- `BeatSpec` 已经具备商业级 tempo map 的表达能力；
- `GridCompiler` 已经解决绝对帧、前填、尾填、接缝去重；
- `fit_rigid` 提供了“脏拍表 → 稳定网格”的鲁棒层；
- 下一步的关键是**自动分段、downbeat、低频音频证据、可配置八度策略**。

如果这四项补齐，HyperMixx 的网格层在数据结构上不会落后于商业产品；差异将主要落在检测前端的精度与曲库规模，而不是网格模型本身。

---

### 附录 A：参考来源

| ID | 来源 | 类型 | URL |
|---|---|---|---|
| S1 | zplane AUFTAKT 官方页（含 Algoriddim CEO、Hercules 引述） | 厂商 SDK 文档 | https://licensing.zplane.de/technology/auftakt |
| S2 | zplane References 合作伙伴列表 | 厂商页面 | https://licensing.zplane.de/references |
| S3 | zplane BARBEATQ 官方页 | 厂商 SDK 文档 | https://licensing.zplane.de/technology/barbeatq |
| S4 | Algoriddim djay Pro 5 新闻稿（Fluid Beatgrid） | 官方新闻稿 | https://www.algoriddim.com/press_releases/447-algoriddim-unveils-djay-pro-5-with-next-generation-neural-mix-crossfader-fusion-and-fluid-beatgrid- |
| S5 | Algoriddim Patents 页面（含 zplane 产品 credits） | 官方页面 | https://www.algoriddim.com/patents |
| S6 | zplane 采访 Mixed In Key 创始人 Yakov Vorobyev（2026-08-04） | 厂商采访 | https://products.zplane.de/blog/yakov-vorobyev-interview-mixed-in-key |
| S7 | Mixed In Key rekordbox 集成页（自研专利 key 算法） | 厂商页面 | https://mixedinkey.com/integration/rekordbox-integration/ |
| S8 | VirtualDJ Changelog（build 9246，2026-03-31） | 官方 changelog | https://virtualdj.com/products/virtualdj/changelog.html |
| S9 | VirtualDJ BPM Editor 用户手册（Rigid / Fluid） | 官方手册 | https://virtualdj.com/manuals/virtualdj/editors/bpmeditor.html |
| S10 | Serato “What's New in Serato DJ 5.0: Lucid Beatgrids” | 官方发布页 | https://the-drop.serato.com/announcements/introducing-lucid-beatgrids-in-serato-dj-5-0/ |
| S11 | Serato 支持：Fixed and Flexible beatgrids | 官方支持文档 | https://support.serato.com/hc/en-us/articles/17557222838543-Fixed-and-Flexible-beatgrids |
| S12 | Serato DJ Pro 5.0.0 Release Notes | 官方发布说明 | https://serato.com/dj/pro/downloads/5.0.0 |
| S13 | Native Instruments 支持：How to Set Beatgrids in TRAKTOR | 官方支持文档 | https://support.native-instruments.com/support/solutions/articles/69000879457-how-to-set-beatgrids-in-traktor |
| S14 | Native Instruments：Introducing flexible beatgrids in Traktor Pro 4 | 官方视频 | https://www.youtube.com/watch?v=l2Rn6dMG0dI |
| S15 | Lexicon：Rekordbox Analysis Settings: Dynamic vs Static & High Precision | 高质量第三方分析 | https://www.lexicondj.com/blog/understanding-rekordbox-beatgrid-analysis |
| S16 | Engine DJ User Guide（v4.3 / v5.1，含 zplane credits） | 官方手册 | https://cdn.inmusicbrands.com/engine/43/Engine%20DJ%20-%20User%20Guide%20-%20v4.3.0.pdf |
| S17 | DJUCED User Manual（zplane SDK 列表，含 AUFTAKT / BARBEATQ） | 官方手册 | https://www.djuced.com/user-manual/ |
| S18 | Google Patents：US6518492B2 System and method of BPM determination | 专利 | https://patents.google.com/patent/US6518492B2/en |
| S19 | Google Patents：US6266003B1 Method and apparatus for signal processing for time-scale and/or pitch modification of audio signals | 专利 | https://patents.google.com/patent/US6266003B1/en |
| S20 | Google Patents：US5614687A Apparatus for detecting the number of beats（Pioneer Electronic Corporation） | 专利 | https://patents.google.com/patent/US5614687A/en |
| S21 | rekordbox 7 功能页（AI vocal 检测） | 官方页面 | https://rekordbox.com/en/feature/overview/ |
| S22 | DJ TechTools：Time-Stretch Comparison（Traktor / Elastique 行业资料） | 第三方资料 | https://djtechtools.com/2009/11/09/time-stretching-comparison-torq-1-5-vs-ableton-live-8/ |
| S23 | DJ.Studio：Understanding Tempo, BPM, and Beat Grids | 官方帮助 | https://help.dj.studio/en/articles/14113156-understanding-tempo-bpm-and-beat-grids-in-dj-studio |
| S24 | Gearjunkies：Mixed In Key First iPhone App – DJ Tempo（使用 aufTAKT） | 第三方报道 | https://www.gearjunkies.com/2010/08/mixed-in-key-first-iphone-app-dj-tempo/ |
| S25 | Reddit r/traktorpro：Traktor Pro 4 Flexible Beat Grids with 70s/80s analog drums | 社区经验 | https://www.reddit.com/r/traktorpro/comments/1ekxism/traktor_pro_4_flexible_beat_grids_with_70_and_80s/ |
| S26 | Engine DJ Community：The beatgrids are mostly out of whack | 社区反馈 | https://community.enginedj.com/t/the-beatgrids-are-mostly-out-of-whack/63193 |
| S27 | Atomix VirtualDJ 旧版产品资料（FAME / BeatLock 功能列表） | 历史资料 | https://archive.org/download/atomix-virtual-dj-infinity-for/Atomix-VirtualDJ-Infinity-for.pdf |

> 访问日期均为 2026-11-07。二手来源仅用于补充产品行为与社区经验，不用于证明底层算法。

---

### 附录 B：证据强度速查

| 结论 | 等级 | 说明 |
|---|---|---|
| djay 授权 zplane AUFTAKT 用于 beat tracking | **A** | CEO 明确陈述 |
| Mixed In Key 的 BPM 使用 AUFTAKT | **A** | 创始人明确陈述 + 历史产品说明 |
| Mixed In Key 的 key 检测为自研专利 | **A** | 创始人 + 官方集成页 |
| zplane AUFTAKT V4 使用 DNN + 统计模型 | **A** | 官方 SDK 页面 |
| djay Fluid Beatgrid = AUFTAKT V4 封装 | **B** | 能力与时间线高度对应，但未逐字确认 |
| Serato Lucid 为自研 | **C** | 无授权证据 + 官方“重建引擎”措辞 |
| Traktor 检测为自研 | **C** | 无 AUFTAKT 证据；zplane 合作集中在 élastique |
| rekordbox 检测为自研 | **C** | 无授权证据；功能表述符合自研 |
| Engine DJ 检测为自研 | **D** | 官方 credits 只列 ELASTIQUE/TONART |
| VirtualDJ 检测为自研 | **C** | 无授权证据；Fluid 数据模型自洽 |
| Serato `US6266003B1` 是节拍检测专利 | **错误** | 实际为信号处理/编码相关 |
| rekordbox 的 AI 用于 beatgrid | **错误** | 主要用于 vocal 检测 |

---

## 九、写在最后：开源与商业的同一张地图

把五套开源实现和七家商业产品放在一起看，会发现它们其实在回答同一组问题，只是**把信任放在不同的地方**：

| 问题 | 开源侧的典型答案 | 商业侧的典型答案 |
|---|---|---|
| 拍从哪来 | onset / novelty + DP / Viterbi | DNN beat activation + 统计后处理（AUFTAKT），或自研引擎 |
| 速度怎么定 | 自相关 / tempogram + 先验 | 多候选 + 曲库调参 + BPM Range |
| 相位怎么定 | 低频包络 / 相位集中度 | 低频重音 + 网格编辑 + 人工修正 |
| 变速怎么办 | 分段 tempo map | 2026 年全面转向 Flexible / Fluid / Dynamic |
| 小节怎么办 | downbeat 证据投票（少数） | 独立 downbeat 阶段 + High Precision |
| 不可信怎么办 | 置信度 / degraded | 可视化编辑 + 锚点 + 手动修正 |

对 HyperMixx 的结论没有变，但更清楚了：

- **数据模型已经对齐商业产品**：`BeatSpec` 的“段 + BPM + 起始帧”就是 tempo map；
- **鲁棒层是差异化**：`fit_rigid` 把脏拍表收敛成稳定网格，这是很多商业产品也需要的后处理；
- **真正的短板在证据链前端**：没有低频音频证据、没有自动分段、没有 downbeat；
- **不要追“AI”标签**：先补可解释的音频证据和可编辑性，比换一个 DNN 更符合当前架构。

一句话：开源项目教你“怎么听”，商业产品教你“怎么让用户修”。HyperMixx 的位置，恰好在这两者中间。
