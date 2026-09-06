//! Native gauges on the taskbar's actual backdrop, with opaque readable text.
use crate::composition::CompositionSurface;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::{Common::*, *};
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::core::{Result, w};
use windows_numerics::Vector2;

#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub label: String,
    pub value: String,
    pub unit: String,
    pub name: String,
    pub extra: String,
    pub progress: Option<f32>,
    pub muted: bool,
}

#[derive(Clone, Copy, PartialEq)]
struct TextSpec {
    width: f32,
    size: f32,
    numeric: bool,
    centered: bool,
}

impl TextSpec {
    fn new(width: f32, size: f32, numeric: bool, centered: bool) -> Self {
        Self {
            width: width.max(1.0),
            size,
            numeric,
            centered,
        }
    }
}

struct CachedText {
    text: String,
    spec: TextSpec,
    layout: IDWriteTextLayout,
}

struct CachedCell {
    label: CachedText,
    value: CachedText,
    unit: CachedText,
    name: CachedText,
    extra: CachedText,
    dial: Option<CachedDial>,
}

struct Tick {
    start: Vector2,
    end: Vector2,
    fraction: f32,
}

struct CachedDial {
    cx: f32,
    cy: f32,
    minimal: bool,
    track: ID2D1PathGeometry,
    // Retain only the current arc, so memory use is bounded by the cell count.
    sweep: Option<f32>,
    progress: Option<ID2D1PathGeometry>,
    ticks: Vec<Tick>,
}

/// Cumulative counters for this renderer instance, including resize/DPI changes.
/// A replacement renderer starts at zero after device-loss recovery.
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct RendererStats {
    pub frames_presented: u64,
    pub text_layouts_created: u64,
    pub text_layouts_reused: u64,
    pub path_geometries_created: u64,
    pub path_geometries_reused: u64,
    pub tick_sets_created: u64,
    pub tick_sets_reused: u64,
}

pub struct Renderer {
    hwnd: HWND,
    cache: Vec<CachedCell>,
    ink: Option<ID2D1SolidColorBrush>,
    surface: CompositionSurface,
    write: IDWriteFactory,
    regular: IDWriteTextFormat,
    numbers: IDWriteTextFormat,
    tabular: IDWriteTypography,
    ellipsis: IDWriteInlineObject,
    wide: Vec<u16>,
    stats: RendererStats,
    dpi: f32,
    cache_style: String,
    column_width: f32,
}

impl Renderer {
    pub unsafe fn new(hwnd: HWND) -> Result<Self> {
        let dpi = GetDpiForWindow(hwnd).max(96) as f32;
        let surface = CompositionSurface::new(hwnd, dpi)?;
        let ink = surface
            .target()
            .CreateSolidColorBrush(&rgba(0xffffff, 1.0), None)?;
        let write: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let regular = write.CreateTextFormat(
            w!("Segoe UI"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            9.0,
            w!("ko-KR"),
        )?;
        let numbers = write.CreateTextFormat(
            w!("Bahnschrift"),
            None,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            13.0,
            w!("ko-KR"),
        )?;
        for f in [&regular, &numbers] {
            f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;
        }
        let tabular = write.CreateTypography()?;
        tabular.AddFontFeature(DWRITE_FONT_FEATURE {
            nameTag: DWRITE_FONT_FEATURE_TAG_TABULAR_FIGURES,
            parameter: 1,
        })?;
        // All previous layouts used this same regular format for their sign.
        let ellipsis = write.CreateEllipsisTrimmingSign(&regular)?;
        Ok(Self {
            hwnd,
            cache: Vec::new(),
            ink: Some(ink),
            surface,
            write,
            regular,
            numbers,
            tabular,
            ellipsis,
            wide: Vec::new(),
            stats: RendererStats::default(),
            dpi,
            cache_style: String::new(),
            column_width: 0.0,
        })
    }

    pub unsafe fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        self.cache.clear();
        self.ink = None;
        self.dpi = GetDpiForWindow(self.hwnd).max(96) as f32;
        self.surface.resize(width, height, self.dpi)?;
        self.ink = Some(
            self.surface
                .target()
                .CreateSolidColorBrush(&rgba(0xffffff, 1.0), None)?,
        );
        Ok(())
    }

    pub fn software_fallback(&self) -> bool {
        self.surface.uses_software_fallback()
    }

    pub fn stats(&self) -> RendererStats {
        self.stats
    }

    unsafe fn layout(&mut self, text: &str, spec: TextSpec) -> Result<IDWriteTextLayout> {
        self.wide.clear();
        self.wide.extend(text.encode_utf16());
        let layout = self.write.CreateTextLayout(
            &self.wide,
            if spec.numeric {
                &self.numbers
            } else {
                &self.regular
            },
            spec.width,
            22.0,
        )?;
        let all = DWRITE_TEXT_RANGE {
            startPosition: 0,
            length: self.wide.len() as u32,
        };
        layout.SetFontSize(spec.size, all)?;
        layout.SetTypography(&self.tabular, all)?;
        if spec.centered {
            layout.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
        }
        if !spec.numeric && spec.size >= 9.0 {
            layout.SetFontWeight(DWRITE_FONT_WEIGHT_SEMI_BOLD, all)?;
        }
        layout.SetTrimming(
            &DWRITE_TRIMMING {
                granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                ..Default::default()
            },
            &self.ellipsis,
        )?;
        self.stats.text_layouts_created += 1;
        Ok(layout)
    }

    unsafe fn cached_text(&mut self, text: &str, spec: TextSpec) -> Result<CachedText> {
        Ok(CachedText {
            layout: self.layout(text, spec)?,
            text: text.into(),
            spec,
        })
    }

    pub unsafe fn draw(&mut self, cells: &[Cell], light: bool, style: &str) -> Result<()> {
        let dpi = GetDpiForWindow(self.hwnd).max(96) as f32;
        if dpi != self.dpi {
            self.dpi = dpi;
            self.surface.set_dpi(dpi);
            self.cache.clear();
        }
        let size = self.surface.target().GetSize();
        let column = (size.width - 8.0) / cells.len().max(1) as f32;
        if self.cache_style != style || (column - self.column_width).abs() > 0.01 {
            self.cache.clear();
            self.column_width = column;
            self.cache_style = style.into();
        }
        self.cache.truncate(cells.len());
        let top = ((size.height - 40.0) / 2.0).max(0.0);
        for (i, cell) in cells.iter().enumerate() {
            let tape = style == "eva";
            let label = TextSpec::new(40.0, 8.0, false, false);
            let value = TextSpec::new(
                if tape { 34.0 } else { 31.0 },
                if tape && cell.value.len() > 3 {
                    14.0
                } else if tape {
                    18.0
                } else {
                    13.0
                },
                true,
                !tape,
            );
            let unit = TextSpec::new(if tape { 18.0 } else { 31.0 }, 7.0, false, !tape);
            let name = TextSpec::new(
                if tape { column - 30.0 } else { column - 43.0 },
                9.0,
                false,
                false,
            );
            let extra = TextSpec::new(
                if tape { column - 49.0 } else { column - 43.0 },
                8.0,
                false,
                false,
            );
            if i == self.cache.len() {
                let cache = CachedCell {
                    label: self.cached_text(&cell.label, label)?,
                    value: self.cached_text(&cell.value, value)?,
                    unit: self.cached_text(&cell.unit, unit)?,
                    name: self.cached_text(&cell.name, name)?,
                    extra: self.cached_text(&cell.extra, extra)?,
                    dial: None,
                };
                self.cache.push(cache);
            } else {
                // Prepare a replacement before borrowing its slot. Unchanged
                // fields keep both their layout and their allocated key string.
                macro_rules! refresh_text {
                    ($field:ident, $spec:ident) => {
                        if self.cache[i].$field.text == cell.$field
                            && self.cache[i].$field.spec == $spec
                        {
                            self.stats.text_layouts_reused += 1;
                        } else {
                            let layout = self.layout(&cell.$field, $spec)?;
                            let cached = &mut self.cache[i].$field;
                            cached.layout = layout;
                            cached.text.clone_from(&cell.$field);
                            cached.spec = $spec;
                        }
                    };
                }
                refresh_text!(label, label);
                refresh_text!(value, value);
                refresh_text!(unit, unit);
                refresh_text!(name, name);
                refresh_text!(extra, extra);
            }
            if !tape {
                // Cache the exact absolute coordinates used by the old draw
                // path; no transform, quantization, or curve approximation.
                self.prepare_dial(i, 4.0 + i as f32 * column, top, cell, style == "minimal")?;
            }
        }
        let target = self.surface.target();
        let ink = self
            .ink
            .as_ref()
            .expect("rebuild renderer after failed resize");
        let p = Palette::new(light, style);
        target.BeginDraw();
        target.Clear(Some(&rgba(0, 0.0)));
        // Every fallible cache allocation is complete before BeginDraw.
        for (i, (cell, cache)) in cells.iter().zip(&self.cache).enumerate() {
            let left = 4.0 + i as f32 * column;
            let accent = if cell.muted {
                p.muted
            } else if cell.progress.is_some_and(|v| v >= 0.9) {
                p.hot
            } else {
                p.accent
            };
            ink.SetColor(&p.tint);
            target.FillRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: rect(left, top + 1.0, left + column - 4.0, top + 39.0),
                    radiusX: 5.0,
                    radiusY: 5.0,
                },
                ink,
            );
            if style == "eva" {
                self.tape(left, top, column, cell, cache, &p, accent);
            } else {
                self.dial(left, top, cell, cache, &p, accent, style == "minimal");
            }
        }
        target.EndDraw(None, None)?;
        self.surface.present()?;
        self.stats.frames_presented += 1;
        Ok(())
    }

    unsafe fn text(&self, text: &CachedText, x: f32, y: f32, color: D2D1_COLOR_F) {
        let ink = self.ink.as_ref().unwrap();
        ink.SetColor(&color);
        self.surface.target().DrawTextLayout(
            Vector2 { X: x, Y: y },
            &text.layout,
            ink,
            D2D1_DRAW_TEXT_OPTIONS_CLIP,
        );
    }

    unsafe fn arc_path(
        &mut self,
        cx: f32,
        cy: f32,
        r: f32,
        start: f32,
        sweep: f32,
    ) -> Result<ID2D1PathGeometry> {
        let path = self.surface.factory().CreatePathGeometry()?;
        let sink = path.Open()?;
        sink.BeginFigure(polar(cx, cy, r, start), D2D1_FIGURE_BEGIN_HOLLOW);
        sink.AddArc(&D2D1_ARC_SEGMENT {
            point: polar(cx, cy, r, start + sweep),
            size: D2D_SIZE_F {
                width: r,
                height: r,
            },
            rotationAngle: 0.0,
            sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
            arcSize: if sweep > 180.0 {
                D2D1_ARC_SIZE_LARGE
            } else {
                D2D1_ARC_SIZE_SMALL
            },
        });
        sink.EndFigure(D2D1_FIGURE_END_OPEN);
        sink.Close()?;
        self.stats.path_geometries_created += 1;
        Ok(path)
    }

    unsafe fn prepare_dial(
        &mut self,
        index: usize,
        left: f32,
        top: f32,
        cell: &Cell,
        minimal: bool,
    ) -> Result<()> {
        let cx = left + 18.0;
        let cy = top + 20.0;
        let sweep = dial_sweep(cell);
        let same_position = self.cache[index]
            .dial
            .as_ref()
            .is_some_and(|dial| dial.cx == cx && dial.cy == cy && dial.minimal == minimal);
        if !same_position {
            let track = self.arc_path(cx, cy, 14.8, 135.0, 270.0)?;
            let progress = if let Some(sweep) = sweep {
                Some(self.arc_path(cx, cy, 14.8, 135.0, sweep)?)
            } else {
                None
            };
            let ticks = dial_ticks(cx, cy, minimal);
            self.cache[index].dial = Some(CachedDial {
                cx,
                cy,
                minimal,
                track,
                sweep,
                progress,
                ticks,
            });
            self.stats.tick_sets_created += 1;
        } else {
            self.stats.path_geometries_reused += 1; // Unchanged track.
            self.stats.tick_sets_reused += 1;
            if self.cache[index].dial.as_ref().unwrap().sweep != sweep {
                let progress = if let Some(sweep) = sweep {
                    Some(self.arc_path(cx, cy, 14.8, 135.0, sweep)?)
                } else {
                    None
                };
                let dial = self.cache[index].dial.as_mut().unwrap();
                dial.progress = progress;
                dial.sweep = sweep;
            } else if sweep.is_some() {
                self.stats.path_geometries_reused += 1;
            }
        }
        Ok(())
    }

    unsafe fn dial(
        &self,
        left: f32,
        top: f32,
        cell: &Cell,
        cache: &CachedCell,
        p: &Palette,
        accent: D2D1_COLOR_F,
        minimal: bool,
    ) {
        let target = self.surface.target();
        let ink = self.ink.as_ref().unwrap();
        let cx = left + 18.0;
        let progress = dial_progress(cell);
        let dial = cache.dial.as_ref().expect("prepare dial before BeginDraw");
        let width = if minimal { 1.25 } else { 2.2 };
        ink.SetColor(&p.track);
        target.DrawGeometry(&dial.track, ink, width, None);
        if let Some(path) = &dial.progress {
            ink.SetColor(&accent);
            target.DrawGeometry(path, ink, width, None);
        }
        for tick in &dial.ticks {
            let active = progress.is_some_and(|v| v >= tick.fraction) && !cell.muted;
            ink.SetColor(&if active { accent } else { p.track });
            target.DrawLine(tick.start, tick.end, ink, 0.6, None);
        }
        self.text(
            &cache.value,
            cx - 15.5,
            top + 10.0,
            if cell.muted { p.muted } else { p.text },
        );
        self.text(&cache.unit, cx - 15.5, top + 27.0, p.subtle);
        self.text(&cache.label, left + 40.0, top + 2.0, accent);
        self.text(
            &cache.name,
            left + 40.0,
            top + 12.5,
            if cell.muted { p.muted } else { p.text },
        );
        self.text(&cache.extra, left + 40.0, top + 25.0, p.subtle);
    }

    unsafe fn tape(
        &self,
        left: f32,
        top: f32,
        column: f32,
        cell: &Cell,
        cache: &CachedCell,
        p: &Palette,
        accent: D2D1_COLOR_F,
    ) {
        let target = self.surface.target();
        let ink = self.ink.as_ref().unwrap();
        self.text(&cache.label, left + 3.0, top + 1.0, accent);
        self.text(
            &cache.name,
            left + 27.0,
            top + 0.5,
            if cell.muted { p.muted } else { p.text },
        );
        self.text(
            &cache.value,
            left + 3.0,
            top + 9.0,
            if cell.muted { p.muted } else { p.text },
        );
        self.text(&cache.unit, left + 37.0, top + 19.0, p.subtle);
        self.text(&cache.extra, left + 49.0, top + 14.0, p.subtle);
        let bar_left = left + 3.0;
        let total = column - 12.0;
        let step = total / 20.0;
        let progress = dial_progress(cell);
        for n in 0..20 {
            let fraction = n as f32 / 20.0;
            ink.SetColor(&p.track);
            target.FillRectangle(
                &rect(
                    bar_left + n as f32 * step,
                    top + 32.0,
                    bar_left + (n + 1) as f32 * step - 1.0,
                    top + 36.0,
                ),
                ink,
            );
            if let Some(v) = progress.filter(|_| !cell.muted) {
                let fill = ((v - fraction) * 20.0).clamp(0.0, 1.0);
                if fill > 0.0 {
                    ink.SetColor(&accent);
                    target.FillRectangle(
                        &rect(
                            bar_left + n as f32 * step,
                            top + 32.0,
                            bar_left + n as f32 * step + (step - 1.0) * fill,
                            top + 36.0,
                        ),
                        ink,
                    );
                }
            }
            if n % 5 == 0 {
                ink.SetColor(&p.subtle);
                target.DrawLine(
                    Vector2 {
                        X: bar_left + n as f32 * step,
                        Y: top + 37.0,
                    },
                    Vector2 {
                        X: bar_left + n as f32 * step,
                        Y: top + 39.0,
                    },
                    ink,
                    0.5,
                    None,
                );
            }
        }
    }
}

fn dial_progress(cell: &Cell) -> Option<f32> {
    cell.progress
        .filter(|v| v.is_finite())
        .map(|v| v.clamp(0.0, 1.0))
}

fn dial_sweep(cell: &Cell) -> Option<f32> {
    dial_progress(cell)
        .filter(|_| !cell.muted)
        .map(|value| 270.0 * value)
        .filter(|sweep| *sweep > 0.0)
}

fn dial_ticks(cx: f32, cy: f32, minimal: bool) -> Vec<Tick> {
    let count = if minimal { 4 } else { 20 };
    (0..=count)
        .map(|n| {
            let fraction = n as f32 / count as f32;
            let a = 135.0 + 270.0 * fraction;
            Tick {
                start: polar(cx, cy, if n % 5 == 0 { 16.7 } else { 17.5 }, a),
                end: polar(cx, cy, 18.5, a),
                fraction,
            }
        })
        .collect()
}

fn rect(left: f32, top: f32, right: f32, bottom: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left,
        top,
        right,
        bottom,
    }
}
fn polar(cx: f32, cy: f32, r: f32, angle: f32) -> Vector2 {
    let a = angle.to_radians();
    Vector2 {
        X: cx + r * a.cos(),
        Y: cy + r * a.sin(),
    }
}
fn rgba(rgb: u32, alpha: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((rgb >> 16) & 255) as f32 / 255.0,
        g: ((rgb >> 8) & 255) as f32 / 255.0,
        b: (rgb & 255) as f32 / 255.0,
        a: alpha,
    }
}
struct Palette {
    text: D2D1_COLOR_F,
    subtle: D2D1_COLOR_F,
    muted: D2D1_COLOR_F,
    track: D2D1_COLOR_F,
    tint: D2D1_COLOR_F,
    accent: D2D1_COLOR_F,
    hot: D2D1_COLOR_F,
}
impl Palette {
    fn new(light: bool, style: &str) -> Self {
        let accent = match (light, style) {
            (true, "eva") => 0xa85120,
            (false, "eva") => 0xffb35c,
            (true, "minimal") => 0x293f4d,
            (false, "minimal") => 0xe8f0ed,
            (true, _) => 0x157779,
            (false, _) => 0x9be5d7,
        };
        Self {
            text: rgba(if light { 0x182f37 } else { 0xf0f5f3 }, 1.0),
            subtle: rgba(if light { 0x314951 } else { 0xbdcfcf }, 0.92),
            muted: rgba(if light { 0x4e646b } else { 0x9caeae }, 0.9),
            track: rgba(if light { 0x264c59 } else { 0xb4d2cf }, 0.25),
            tint: rgba(
                if light { 0xffffff } else { 0x0c1b20 },
                if style == "minimal" { 0.015 } else { 0.07 },
            ),
            accent: rgba(accent, 1.0),
            hot: rgba(if light { 0xa84928 } else { 0xffb37c }, 1.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(progress: Option<f32>, muted: bool) -> Cell {
        Cell {
            label: String::new(),
            value: String::new(),
            unit: String::new(),
            name: String::new(),
            extra: String::new(),
            progress,
            muted,
        }
    }

    #[test]
    fn invalid_samples_never_create_non_finite_geometry() {
        for progress in [
            None,
            Some(f32::NAN),
            Some(f32::INFINITY),
            Some(f32::NEG_INFINITY),
        ] {
            let cell = sample(progress, false);
            assert_eq!(dial_progress(&cell), None);
            assert_eq!(dial_sweep(&cell), None);
        }
        assert_eq!(dial_sweep(&sample(Some(0.5), false)), Some(135.0));
        assert_eq!(dial_sweep(&sample(Some(f32::MAX), false)), Some(270.0));
        assert_eq!(dial_sweep(&sample(Some(-1.0), false)), None);
    }

    #[test]
    fn unavailable_transition_removes_arc_even_when_value_is_unchanged() {
        let mut cell = sample(Some(0.5), false);
        let ready = dial_sweep(&cell);
        cell.muted = true;
        assert_eq!(dial_sweep(&cell), None);
        // The cached sweep must change again when the collector becomes ready.
        cell.muted = false;
        assert_eq!(dial_sweep(&cell), ready);
        assert_eq!(ready, Some(135.0));
    }

    #[test]
    fn zero_percent_keeps_first_tick_without_a_degenerate_arc() {
        let cell = sample(Some(0.0), false);
        let ticks = dial_ticks(22.0, 20.0, false);
        assert_eq!(dial_sweep(&cell), None);
        let active = ticks
            .iter()
            .filter(|tick| dial_progress(&cell).is_some_and(|value| value >= tick.fraction))
            .count();
        assert_eq!(active, 1);
    }

    fn near(actual: Vector2, x: f32, y: f32) {
        assert!((actual.X - x).abs() < 0.0001, "x = {}", actual.X);
        assert!((actual.Y - y).abs() < 0.0001, "y = {}", actual.Y);
    }

    #[test]
    fn cached_ticks_preserve_hud_and_minimal_endpoints() {
        let hud = dial_ticks(22.0, 20.0, false);
        let minimal = dial_ticks(22.0, 20.0, true);
        assert_eq!(hud.len(), 21);
        assert_eq!(minimal.len(), 5);
        near(hud[0].start, 10.191316, 31.808685);
        near(hud[0].end, 8.918525, 33.081474);
        near(hud[20].start, 33.808685, 31.808685);
        near(hud[20].end, 35.081474, 33.081474);
        // Minimal's final index is 4, so it retains the short-tick radius.
        near(minimal[4].start, 34.37437, 32.37437);
        near(minimal[4].end, 35.081474, 33.081474);
        assert_eq!(hud[0].fraction, 0.0);
        assert_eq!(hud[20].fraction, 1.0);
        assert_eq!(minimal[4].fraction, 1.0);
    }
}
