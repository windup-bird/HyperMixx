# preset / unit 速查（已迁移）

这份文档描述的旧 schema（`shape`、`[[unit.selector]]`、`bind` 平铺表、`step`/`beats`/`seconds` 字段）
**已经不存在**。

现行 schema 见 [`docs/MIDI.md`](docs/MIDI.md)，目标结构见 `plans/1028unitplan.md`。

一句话版：

```toml
[[unit]]
id = "deck1.mixer"
preset = "mixer"
deck = 1

[[unit.control]]
slot = "filter"
endpoint = { midi = "cc", channel = 0, id = 12 }

[[unit.mode]]
name = "default"
[[unit.mode.bind]]
slot = "filter"
action = "fx param value"
fx = "filter"
```
