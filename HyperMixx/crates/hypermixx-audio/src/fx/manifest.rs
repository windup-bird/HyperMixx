//! 效果静态清单：Flutter 桥 / 未来 UI 枚举效果与参数的唯一入口。
//! 不可变 'static，与处理器解耦（实现方案.md 决策 #4）；
//! 参数载自然单位（s/Hz/ms/dB/beats/ratio），总线 p1..p4 按位对应。

use super::EffectId;

#[derive(Clone, Copy, Debug)]
pub enum ParamKind {
    Continuous { min: f32, max: f32 },
    /// 离散步进（mode、sync 开关、拍步长等）。
    Stepped { min: f32, max: f32, step: f32 },
}

#[derive(Clone, Copy, Debug)]
pub struct ParamSpec {
    /// 稳定标识（桥/总线侧用）。
    pub name: &'static str,
    /// 显示名。
    pub label: &'static str,
    pub unit: &'static str,
    pub kind: ParamKind,
    pub default: f32,
}

impl ParamSpec {
    /// clamp 到声明范围（Stepped 同时吸附到步长）。
    pub fn clamp(&self, v: f32) -> f32 {
        match self.kind {
            ParamKind::Continuous { min, max } => v.clamp(min, max),
            ParamKind::Stepped { min, max, step } => {
                (((v - min) / step).round() * step + min).clamp(min, max)
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct EffectManifest {
    pub id: EffectId,
    /// 稳定标识（如 "echo"）。
    pub name: &'static str,
    pub label: &'static str,
    /// 0..=4 条，按位对应总线 p1..p4。
    pub params: &'static [ParamSpec],
}

const fn cont(min: f32, max: f32) -> ParamKind {
    ParamKind::Continuous { min, max }
}
const fn stepped(min: f32, max: f32, step: f32) -> ParamKind {
    ParamKind::Stepped { min, max, step }
}
const fn p(
    name: &'static str,
    label: &'static str,
    unit: &'static str,
    kind: ParamKind,
    default: f32,
) -> ParamSpec {
    ParamSpec {
        name,
        label,
        unit,
        kind,
        default,
    }
}

/// 8 个效果，顺序与 EffectId 判别值一致（1..=8）。
static MANIFESTS: [EffectManifest; 8] = [
    EffectManifest {
        id: EffectId::Echo,
        name: "echo",
        label: "回声",
        params: &[
            p("time", "Time", "s", cont(0.01, 2.0), 0.375),
            p("feedback", "Feedback", "ratio", cont(0.0, 0.95), 0.35),
            p("damp", "Damp", "ratio", cont(0.0, 1.0), 0.3),
            p("sync", "Sync", "", stepped(0.0, 1.0, 1.0), 0.0),
        ],
    },
    EffectManifest {
        id: EffectId::Flanger,
        name: "flanger",
        label: "镶边",
        params: &[
            p("rate", "Rate", "Hz", cont(0.05, 5.0), 0.5),
            p("base", "Base", "ms", cont(0.2, 8.0), 2.0),
            p("depth", "Depth", "ms", cont(0.0, 6.0), 3.0),
            p("feedback", "Feedback", "ratio", cont(-0.9, 0.9), 0.4),
        ],
    },
    EffectManifest {
        id: EffectId::Phaser,
        name: "phaser",
        label: "移相",
        params: &[
            p("rate", "Rate", "Hz", cont(0.05, 5.0), 0.5),
            p("base", "Base", "Hz", cont(100.0, 4000.0), 800.0),
            p("depth", "Depth", "ratio", cont(0.0, 1.0), 0.5),
            p("feedback", "Feedback", "ratio", cont(-0.9, 0.9), 0.3),
        ],
    },
    EffectManifest {
        id: EffectId::Reverb,
        name: "reverb",
        label: "混响",
        params: &[
            p("roomsize", "Room Size", "ratio", cont(0.0, 1.0), 0.5),
            p("damp", "Damp", "ratio", cont(0.0, 1.0), 0.3),
            p("width", "Width", "ratio", cont(0.0, 1.0), 0.7),
        ],
    },
    EffectManifest {
        id: EffectId::Distortion,
        name: "distortion",
        label: "失真",
        params: &[
            p("drive", "Drive", "dB", cont(0.0, 40.0), 12.0),
            p("makeup", "Makeup", "dB", cont(-12.0, 12.0), 0.0),
        ],
    },
    EffectManifest {
        id: EffectId::FilterLinear,
        name: "filter",
        label: "滤波",
        params: &[
            p("mode", "Mode", "", stepped(0.0, 2.0, 1.0), 0.0),
            p("cutoff", "Cutoff", "Hz", cont(20.0, 20000.0), 1000.0),
            p("q", "Q", "ratio", cont(0.5, 16.0), 0.707),
        ],
    },
    EffectManifest {
        id: EffectId::FilterMoog,
        name: "moog",
        label: "Moog",
        params: &[
            p("cutoff", "Cutoff", "Hz", cont(20.0, 20000.0), 2000.0),
            p("res", "Resonance", "ratio", cont(0.0, 1.0), 0.2),
            p("drive", "Drive", "dB", cont(0.0, 24.0), 0.0),
        ],
    },
    EffectManifest {
        id: EffectId::Gate,
        name: "gate",
        label: "门控",
        params: &[
            p("period", "Period", "beats", stepped(0.25, 8.0, 0.25), 1.0),
            p("duty", "Duty", "ratio", cont(0.05, 0.95), 0.5),
            p("smooth", "Smooth", "ms", cont(1.0, 50.0), 5.0),
            p("offset", "Offset", "ratio", cont(0.0, 1.0), 0.0),
        ],
    },
];

/// 全部效果清单（固定顺序 = EffectId 1..=8；Flutter 桥枚举入口）。
pub fn all_manifests() -> &'static [EffectManifest] {
    &MANIFESTS
}

/// 按 id 取清单；EffectId::None 会 panic（调用方先判空）。
pub fn manifest(id: EffectId) -> &'static EffectManifest {
    MANIFESTS
        .iter()
        .find(|m| m.id == id)
        .expect("EffectId::None 无 manifest")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifests_cover_all_ids_in_discriminant_order() {
        for (i, m) in MANIFESTS.iter().enumerate() {
            assert_eq!(m.id.to_bus() as usize, i + 1, "清单顺序须与 EffectId 一致");
        }
        assert_eq!(all_manifests().len(), 8);
    }

    #[test]
    fn every_param_has_valid_default() {
        for m in MANIFESTS.iter() {
            assert!(m.params.len() <= 4, "{} 参数超过 4 条", m.name);
            for spec in m.params {
                let c = spec.clamp(spec.default);
                assert!((c - spec.default).abs() < 1e-4, "{} 默认值越界", spec.name);
            }
        }
    }

    #[test]
    fn stepped_clamp_snaps() {
        let period = &MANIFESTS[7].params[0];
        assert_eq!(period.clamp(0.4), 0.5, "period 吸附 1/4 拍");
        assert_eq!(period.clamp(0.13), 0.25);
        assert_eq!(period.clamp(9.0), 8.0, "上限 clamp");
    }
}
