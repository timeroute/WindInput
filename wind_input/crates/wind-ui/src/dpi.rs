//! DPI 缩放：按目标显示器实时计算，支持多显示器不同缩放的动态切换。
//!
//! Per-Monitor DPI 感知下，窗口跨显示器后缩放因子会变化。旧实现用
//! `GetDC(HWND::default())` 取的是**主显示器** DPI，且只在窗口 `new()` 时读一次缓存，
//! 换显示器后既拿不到目标显示器 DPI、也不会更新 —— 这就是"切显示器缩放不对"的根因。
//!
//! 这里按"窗口将要显示的点"实时取其所在显示器的有效 DPI；各瞬态窗口（候选/气泡/菜单）
//! 在 `show()` 时调用即可随光标所在显示器自动适配。

/// 取得点 (x, y) 所在显示器的有效 DPI 缩放（96dpi = 1.0）。
/// 失败回退 1.0。
#[cfg(windows)]
pub fn scale_for_point(x: i32, y: i32) -> f32 {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromPoint};
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    unsafe {
        let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut dpi_x: u32 = 0;
        let mut dpi_y: u32 = 0;
        if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_ok() && dpi_y > 0 {
            return dpi_y as f32 / 96.0;
        }
        1.0
    }
}

/// macOS：取点 (x,y) 所在显示器的 backing 缩放（Retina = 2.0）。
///
/// 用 Core Graphics 的 `CGDisplay`（**不依赖主线程**，故可在 forwarder 工作线程调用——
/// 这正是 render_frame 的运行线程；`NSScreen::backingScaleFactor` 需主线程，在此恒失败回退 1.0，
/// 故弃用）。缩放 = 像素宽 / 点宽（`pixels_wide / bounds.width`）。
/// 命中点的显示器；无命中（点在屏外）或取屏失败回退主屏，再失败回退 1.0。
#[cfg(target_os = "macos")]
pub fn scale_for_point(x: i32, y: i32) -> f32 {
    use core_graphics::display::CGDisplay;
    use core_graphics::geometry::CGPoint;
    let display = CGDisplay::displays_with_point(CGPoint::new(x as f64, y as f64), 1)
        .ok()
        .and_then(|(ids, _count)| ids.first().copied())
        .map(CGDisplay::new)
        .unwrap_or_else(CGDisplay::main);
    // 注意：CGDisplayPixelsWide（display.pixels_wide()）返回的是【点】宽，Retina 上不是
    // 原生像素，故 pixels_wide/bounds.width 恒=1 → 候选框按 1x 渲染、Retina 上模糊。
    // 正确：用当前显示【模式】的原生像素宽 / 点宽得 backing 倍率（Retina=2）。
    if let Some(mode) = display.display_mode() {
        let px = mode.pixel_width() as f64;
        let pt = mode.width() as f64;
        if pt > 0.0 {
            let s = (px / pt) as f32;
            if s >= 1.0 {
                return s;
            }
        }
    }
    1.0
}

/// Linux 等：服务不链接任何桌面库，缩放由宿主（Fcitx5 addon）按当前焦点所在的显示环境
/// 上报（`host.display` 扩展信封），这里只存最新值。**不分显示器**：addon 已按焦点 / 候选窗所在
/// 的输出取好那一个，服务端无从也无需再按点查。未上报过按 1.0（等同旧行为）。
#[cfg(all(not(windows), not(target_os = "macos")))]
mod host_scale {
    use std::sync::atomic::{AtomicU32, Ordering};

    /// 缩放因子的 f32 位模式。原子量而非锁：渲染线程每帧都读，上报在 IPC 线程写。
    static BITS: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0f32

    /// 合法范围。下限 1.0：Xft.dpi=72 之类不该把界面缩小到比设计尺寸还小；上限 4.0 防坏值把
    /// 位图撑爆（候选帧宽高另有 16384 的硬上限，这里只是别让一个坏数字先走到那一步）。
    pub const MIN: f32 = 1.0;
    pub const MAX: f32 = 4.0;

    pub fn set(scale: f32) {
        if scale.is_finite() {
            BITS.store(scale.clamp(MIN, MAX).to_bits(), Ordering::Relaxed);
        }
    }

    pub fn get() -> f32 {
        f32::from_bits(BITS.load(Ordering::Relaxed))
    }
}

#[cfg(all(not(windows), not(target_os = "macos")))]
pub use host_scale::set as set_host_scale;

#[cfg(all(not(windows), not(target_os = "macos")))]
pub fn scale_for_point(_x: i32, _y: i32) -> f32 {
    host_scale::get()
}

#[cfg(all(not(windows), not(target_os = "macos")))]
#[cfg(test)]
mod host_scale_tests {
    use super::*;

    #[test]
    fn host_scale_is_clamped_and_ignores_garbage() {
        set_host_scale(1.5);
        assert_eq!(scale_for_point(0, 0), 1.5);
        set_host_scale(0.75);
        assert_eq!(scale_for_point(0, 0), 1.0, "下限 1.0");
        set_host_scale(99.0);
        assert_eq!(scale_for_point(0, 0), 4.0, "上限 4.0");
        set_host_scale(f32::NAN);
        assert_eq!(scale_for_point(0, 0), 4.0, "NaN 不改现值");
        set_host_scale(1.0); // 全局量：别把状态留给同进程的其他测试
    }
}

#[cfg(target_os = "macos")]
#[cfg(test)]
mod tests {
    use super::*;

    /// 冒烟：缩放至少 1.0（CGDisplay 不依赖主线程，Retina 上应为 2.0，非 Retina 1.0）。
    #[test]
    fn scale_for_point_at_least_one() {
        let s = scale_for_point(0, 0);
        assert!(s >= 1.0, "scale 应 >= 1.0，实际 {s}");
    }
}
