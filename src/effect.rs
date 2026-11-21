//! Matrix "digital rain" renderer: turns camera frames into falling green glyphs.

const ATLAS: &[u8] = include_bytes!("../assets/glyphs.bin");
const ATLAS_W: usize = 24;
const ATLAS_H: usize = 36;
const DROPS: usize = 2; // rain drops per column

struct Drop {
    head: f32,   // row of the leading glyph (wraps at `period`)
    period: f32, // longer than the screen, so there are gaps between passes
    speed: f32,  // rows per second
    trail: f32,  // rows
}

pub struct MatrixEffect {
    width: usize,
    height: usize,
    cw: usize,
    ch: usize,
    cols: usize,
    rows: usize,
    glyphs: Vec<f32>, // n * ch * cw coverage masks, 0..1
    n_glyphs: usize,
    chars: Vec<usize>,
    drops: Vec<Drop>, // cols * DROPS
    luma: Vec<f32>,
    lo: f32,
    hi: f32,
    green: Vec<f32>,
    white: Vec<f32>,
    glow: Vec<f32>,
    tmp: Vec<f32>,
    rng: fastrand::Rng,
}

impl MatrixEffect {
    pub fn new(width: usize, height: usize, cell_height: usize) -> Self {
        let ch = cell_height.clamp(6, height.max(6));
        let cw = ((ch * 2 + 1) / 3).max(4);
        let (cols, rows) = (width / cw, height / ch);
        let mut rng = fastrand::Rng::new();

        // Area-average the atlas down (or up) to the cell size; boost thin strokes.
        let n_glyphs = ATLAS.len() / (ATLAS_W * ATLAS_H);
        let mut glyphs = vec![0f32; n_glyphs * ch * cw];
        for g in 0..n_glyphs {
            let src = &ATLAS[g * ATLAS_W * ATLAS_H..(g + 1) * ATLAS_W * ATLAS_H];
            for y in 0..ch {
                let (y0, y1) = (y * ATLAS_H / ch, ((y + 1) * ATLAS_H / ch).max(y * ATLAS_H / ch + 1));
                for x in 0..cw {
                    let (x0, x1) = (x * ATLAS_W / cw, ((x + 1) * ATLAS_W / cw).max(x * ATLAS_W / cw + 1));
                    let mut sum = 0u32;
                    for sy in y0..y1.min(ATLAS_H) {
                        for sx in x0..x1.min(ATLAS_W) {
                            sum += src[sy * ATLAS_W + sx] as u32;
                        }
                    }
                    let avg = sum as f32 / ((y1 - y0) * (x1 - x0)) as f32 / 255.0;
                    glyphs[(g * ch + y) * cw + x] = avg;
                }
            }
        }
        let max = glyphs.iter().cloned().fold(1e-6f32, f32::max);
        glyphs.iter_mut().for_each(|v| *v = (*v / max).powf(0.55));

        let rows_f = rows as f32;
        let drops = (0..cols * DROPS)
            .map(|_| {
                let period = (1.3 + 1.1 * rng.f32()) * rows_f;
                Drop {
                    head: rng.f32() * period,
                    period,
                    speed: 7.0 + 15.0 * rng.f32(),
                    trail: (0.35 + 0.55 * rng.f32()) * rows_f,
                }
            })
            .collect();
        let chars = (0..cols * rows).map(|_| rng.usize(..n_glyphs)).collect();

        let (gw, gh) = (cols * cw, rows * ch);
        MatrixEffect {
            width,
            height,
            cw,
            ch,
            cols,
            rows,
            glyphs,
            n_glyphs,
            chars,
            drops,
            luma: Vec::new(),
            lo: 0.0,
            hi: 1.0,
            green: vec![0.0; gw * gh],
            white: vec![0.0; gw * gh],
            glow: vec![0.0; (gw / 2) * (gh / 2)],
            tmp: vec![0.0; (gw / 2) * (gh / 2)],
            rng,
        }
    }

    /// Average the source image down to one brightness value per glyph cell.
    fn cell_luma(&self, rgb: &[u8], sw: usize, sh: usize) -> Vec<f32> {
        let (cols, rows) = (self.cols, self.rows);
        let mut sum = vec![0u32; cols * rows];
        let mut cnt = vec![0u32; cols * rows];
        let step = (sw / (cols * 3)).max(1); // ~3 samples per cell per axis is plenty
        for y in (0..sh).step_by(step) {
            let r = y * rows / sh;
            let line = &rgb[y * sw * 3..(y + 1) * sw * 3];
            for x in (0..sw).step_by(step) {
                let p = &line[x * 3..x * 3 + 3];
                let l = (p[0] as u32 * 77 + p[1] as u32 * 150 + p[2] as u32 * 29) >> 8;
                let i = r * cols + x * cols / sw;
                sum[i] += l;
                cnt[i] += 1;
            }
        }
        sum.iter().zip(&cnt).map(|(&s, &c)| s as f32 / (c.max(1) as f32 * 255.0)).collect()
    }

    /// Render one frame. `rgb` is the tightly packed source image (sw x sh),
    /// `out` receives width*height*3 bytes of RGB.
    pub fn render(&mut self, rgb: &[u8], sw: usize, sh: usize, dt: f32, out: &mut [u8]) {
        let (cols, rows, cw, ch) = (self.cols, self.rows, self.cw, self.ch);
        let (gw, gh) = (cols * cw, rows * ch);

        // --- camera brightness per cell, auto-contrast, temporal smoothing ---
        let mut luma = self.cell_luma(rgb, sw, sh);
        let mut sorted = luma.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let lo = sorted[sorted.len() / 50];
        let hi = sorted[sorted.len() - 1 - sorted.len() / 50];
        if self.luma.is_empty() {
            (self.lo, self.hi) = (lo, hi);
        } else {
            self.lo += 0.1 * (lo - self.lo);
            self.hi += 0.1 * (hi - self.hi);
        }
        let range = (self.hi - self.lo).max(0.15);
        for v in luma.iter_mut() {
            *v = ((*v - self.lo) / range).clamp(0.0, 1.0).powf(1.15);
        }
        if self.luma.len() == luma.len() {
            for (s, v) in self.luma.iter_mut().zip(&luma) {
                *s = 0.5 * *s + 0.5 * v; // keeps sensor noise from making glyphs flicker
            }
        } else {
            self.luma = luma;
        }

        // --- advance the rain ---
        for d in self.drops.iter_mut() {
            d.head = (d.head + d.speed * dt) % d.period;
        }

        // --- draw glyph cells ---
        let shimmer = 0.6 * dt;
        for r in 0..rows {
            for c in 0..cols {
                let mut rain = 0f32;
                let mut head = false;
                for d in &self.drops[c * DROPS..(c + 1) * DROPS] {
                    let dist = (d.head - r as f32).rem_euclid(d.period); // rows behind the head
                    rain = rain.max((1.0 - dist / d.trail).clamp(0.0, 1.0).powf(1.5));
                    head |= dist < 1.0;
                }
                let i = r * cols + c;
                if head || self.rng.f32() < shimmer {
                    self.chars[i] = self.rng.usize(..self.n_glyphs);
                }
                // The camera image shows through as glyph brightness; rain sweeps over it.
                let l = self.luma[i];
                let mut intensity = l * (0.6 + 0.9 * rain) + 0.12 * rain;
                let mut white = 0.0;
                if head {
                    intensity = (intensity * 1.6).max(0.35 + 0.65 * l);
                    white = 0.75;
                }
                white = (white + 0.35 * (intensity - 0.75).clamp(0.0, 1.0)).min(1.0);
                let intensity = intensity.min(1.3);

                let glyph = &self.glyphs[self.chars[i] * ch * cw..(self.chars[i] + 1) * ch * cw];
                for y in 0..ch {
                    let o = (r * ch + y) * gw + c * cw;
                    let g = &glyph[y * cw..(y + 1) * cw];
                    for x in 0..cw {
                        let v = g[x] * intensity;
                        self.green[o + x] = v;
                        self.white[o + x] = v * white;
                    }
                }
            }
        }

        // --- soft glow: half-res copy, two box-blur passes, added back bilinearly ---
        let (hw, hh) = (gw / 2, gh / 2);
        for y in 0..hh {
            for x in 0..hw {
                let o = y * 2 * gw + x * 2;
                self.glow[y * hw + x] =
                    0.25 * (self.green[o] + self.green[o + 1] + self.green[o + gw] + self.green[o + gw + 1]);
            }
        }
        for _ in 0..2 {
            box_blur_h(&self.glow, &mut self.tmp, hw, hh, 3);
            box_blur_v(&self.tmp, &mut self.glow, hw, hh, 3);
        }

        // --- compose into the output frame (centred, black border) ---
        out.fill(0);
        let (x0, y0) = ((self.width - gw) / 2, (self.height - gh) / 2);
        for y in 0..gh {
            let gy = (y / 2).min(hh - 1);
            let row = &mut out[((y0 + y) * self.width + x0) * 3..((y0 + y) * self.width + x0 + gw) * 3];
            for x in 0..gw {
                let glow = self.glow[gy * hw + (x / 2).min(hw - 1)];
                let g = self.green[y * gw + x] + 0.45 * glow;
                let w = (self.white[y * gw + x] * 255.0).min(255.0) as u8;
                row[x * 3] = w;
                row[x * 3 + 1] = (g * 255.0).min(255.0) as u8;
                row[x * 3 + 2] = w;
            }
        }
    }
}

fn box_blur_h(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: usize) {
    let norm = 1.0 / (2 * r + 1) as f32;
    for y in 0..h {
        let line = &src[y * w..(y + 1) * w];
        let mut acc: f32 = (0..=r.min(w - 1)).map(|x| line[x]).sum();
        for x in 0..w {
            dst[y * w + x] = acc * norm;
            if x + r + 1 < w {
                acc += line[x + r + 1];
            }
            if x >= r {
                acc -= line[x - r];
            }
        }
    }
}

fn box_blur_v(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: usize) {
    let norm = 1.0 / (2 * r + 1) as f32;
    let mut acc = vec![0f32; w];
    for y in 0..=r.min(h - 1) {
        for x in 0..w {
            acc[x] += src[y * w + x];
        }
    }
    for y in 0..h {
        for x in 0..w {
            dst[y * w + x] = acc[x] * norm;
        }
        if y + r + 1 < h {
            for x in 0..w {
                acc[x] += src[(y + r + 1) * w + x];
            }
        }
        if y >= r {
            for x in 0..w {
                acc[x] -= src[(y - r) * w + x];
            }
        }
    }
}
