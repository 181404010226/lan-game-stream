use stream_wire::ContentInsets;

pub const EDGE_SAMPLES: usize = 48;

pub fn near_middle(length: usize, sample: usize) -> usize {
    length / 14 + sample * (length * 6 / 7) / EDGE_SAMPLES
}

#[derive(Default)]
pub struct BlackBars {
    current: ContentInsets,
    candidate: ContentInsets,
    observations: u8,
}

impl BlackBars {
    pub fn current(&self) -> ContentInsets {
        self.current
    }

    pub fn observe(&mut self, bgra: &[u8], width: u32, height: u32) {
        let width = width as usize;
        let height = height as usize;
        let bright = |x, y| is_bright(bgra, x, y, width);
        self.observe_edges(
            width,
            height,
            |x, n| bright(x, near_middle(height, n)),
            |y, n| bright(near_middle(width, n), y),
        );
    }

    pub fn observe_edges(
        &mut self,
        width: usize,
        height: usize,
        row: impl Fn(usize, usize) -> bool,
        column: impl Fn(usize, usize) -> bool,
    ) {
        let observed = ContentInsets {
            x: fraction(paired_margin(width, row), width),
            y: fraction(paired_margin(height, column), height),
        };
        if close(observed, self.current) {
            self.observations = 0;
            return;
        }
        if close(observed, self.candidate) {
            self.observations = self.observations.saturating_add(1);
        } else {
            self.candidate = observed;
            self.observations = 1;
        }
        let leaving = observed.x < self.current.x || observed.y < self.current.y;
        if self.observations >= if leaving { 2 } else { 3 } {
            self.current = self.candidate;
            self.observations = 0;
        }
    }
}

pub fn is_bright(bgra: &[u8], x: usize, y: usize, stride_pixels: usize) -> bool {
    let offset = (y * stride_pixels + x) * 4;
    bgra[offset..offset + 3].iter().any(|color| *color > 8)
}

fn fraction(part: usize, whole: usize) -> u16 {
    (part as u64 * u64::from(u16::MAX) / whole as u64) as u16
}

fn close(a: ContentInsets, b: ContentInsets) -> bool {
    a.x.abs_diff(b.x) <= 180 && a.y.abs_diff(b.y) <= 180
}

fn paired_margin(length: usize, bright: impl Fn(usize, usize) -> bool) -> usize {
    let step = length.div_ceil(600);
    let limit = length / 4;
    let nonblack = |position: usize| {
        (0..EDGE_SAMPLES)
            .filter(|&sample| bright(position, sample))
            .count()
    };
    let measure = |reverse: bool| -> Option<usize> {
        let pixel = |offset| if reverse { length - 1 - offset } else { offset };
        let border = (0..limit)
            .step_by(step)
            .find(|offset| nonblack(pixel(*offset)) > 3)?;
        let min_margin = (length / 60).max(3);
        if border < min_margin || nonblack(pixel(border + (length / 100).max(2))) < 10 {
            return None;
        }
        Some(border)
    };
    match (measure(false), measure(true)) {
        (Some(a), Some(b)) if a.abs_diff(b) <= (length / 80).max(step * 2) => {
            a.min(b).saturating_sub(step)
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: usize, h: usize, x: usize, y: usize) -> Vec<u8> {
        let mut pixels = vec![0; w * h * 4];
        for row in y..h - y {
            for col in x..w - x {
                let start = (row * w + col) * 4;
                pixels[start..start + 3].fill(90);
            }
        }
        pixels
    }

    #[test]
    fn confirms_bars_and_restores_content_when_picture_expands() {
        let mut tracker = BlackBars::default();
        let wide = frame(800, 360, 80, 0);
        tracker.observe(&wide, 800, 360);
        assert_eq!(tracker.current(), ContentInsets::default());
        for _ in 0..2 {
            tracker.observe(&wide, 800, 360);
        }
        assert!(tracker.current().x > 6000);
        assert_eq!(tracker.current().y, 0);
        let fills = frame(800, 360, 0, 0);
        for _ in 0..2 {
            tracker.observe(&fills, 800, 360);
        }
        assert_eq!(tracker.current(), ContentInsets::default());
    }

    #[test]
    fn ignores_black_loading_frames_and_single_dark_edge() {
        let mut tracker = BlackBars::default();
        let black = vec![0; 640 * 360 * 4];
        let mut one_edge = frame(640, 360, 0, 0);
        for row in one_edge.chunks_exact_mut(640 * 4) {
            row[..48 * 4].fill(0);
        }
        for _ in 0..3 {
            tracker.observe(&black, 640, 360);
            tracker.observe(&one_edge, 640, 360);
        }
        assert_eq!(tracker.current(), ContentInsets::default());
    }

    #[test]
    fn top_and_bottom_bars_ignore_score_text_in_corner() {
        let mut tracker = BlackBars::default();
        let mut pixels = frame(640, 480, 0, 60);
        for row in pixels.chunks_exact_mut(640 * 4).take(12) {
            row[..28 * 4].fill(170);
        }
        for _ in 0..3 {
            tracker.observe(&pixels, 640, 480);
        }
        assert_eq!(tracker.current().x, 0);
        assert!(tracker.current().y > 7800);
    }
}
