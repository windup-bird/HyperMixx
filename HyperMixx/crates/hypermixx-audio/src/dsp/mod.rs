//! DSP 基础件：双二阶滤波器、三段 EQ、参数平滑。
//! 全部零分配、f32、按样本处理；算法参考 RBJ Cookbook 与 mixi 的 dsp/ 结构重新实现。

pub mod biquad;
pub mod deck_filter;
pub mod eq;
pub mod pitch;
pub mod smoother;

/// 清除 denormal / NaN / Inf（每块输出后调用一次，作为 FTZ 之外的兜底）。
pub fn sanitize(block: &mut [f32]) {
    for v in block.iter_mut() {
        if !v.is_finite() || v.abs() < 1e-30 {
            *v = 0.0;
        }
    }
}

/// 在音频线程首次进入时开启 FTZ/DAZ（尽力而为，失败无害）。
pub fn enable_ftz() {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::asm;
        unsafe {
            // stmxcsr/ldmxcsr 只接受内存操作数；FTZ(bit15) | DAZ(bit6)
            let mut mxcsr: u32 = 0;
            asm!(
                "stmxcsr [{0}]",
                "or dword ptr [{0}], 0x8040",
                "ldmxcsr [{0}]",
                in(reg) &mut mxcsr,
                options(nostack)
            );
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        use std::arch::asm;
        unsafe {
            // FPCR FZ = bit 24
            asm!(
                "mrs x0, fpcr",
                "orr x0, x0, #0x01000000",
                "msr fpcr, x0",
                out("x0") _,
                options(nostack)
            );
        }
    }
}
