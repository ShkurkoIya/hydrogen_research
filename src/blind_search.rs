#[derive(Clone, Debug)]
pub struct AnomalyCandidate {
    pub cx: usize,
    pub cy: usize,
    pub delta_ndvi: f64,
    pub core_ndvi: f64,
    pub bg_ndvi: f64,
    pub z_score: f64,
    pub symmetry_diff: f64,
    pub dist_to_well_m: f64,
}

/// Быстрая интегральная таблица (Summed-Area Table) для подсчета средних за O(1)
struct IntegralImage {
    w_pad: usize,
    sum: Vec<f64>,
    cnt: Vec<u32>,
}

impl IntegralImage {
    fn new(grid: &[f64], width: usize, height: usize) -> Self {
        let w_pad = width + 1;
        let h_pad = height + 1;
        let mut sum = vec![0.0_f64; w_pad * h_pad];
        let mut cnt = vec![0_u32; w_pad * h_pad];

        for y in 0..height {
            for x in 0..width {
                let val = grid[y * width + x];
                let (v, c) = if val.is_nan() { (0.0, 0) } else { (val, 1) };

                let idx_cur = (y + 1) * w_pad + (x + 1);
                let idx_top = y * w_pad + (x + 1);
                let idx_left = (y + 1) * w_pad + x;
                let idx_tl = y * w_pad + x;

                sum[idx_cur] = v + sum[idx_top] + sum[idx_left] - sum[idx_tl];
                cnt[idx_cur] = c + cnt[idx_top] + cnt[idx_left] - cnt[idx_tl];
            }
        }

        Self { w_pad, sum, cnt }
    }

    /// Возвращает (сумму значений, количество валидных пикселей) в прямоугольнике за O(1)
    fn query(&self, x1: usize, y1: usize, x2: usize, y2: usize) -> (f64, u32) {
        let a = y1 * self.w_pad + x1;
        let b = y1 * self.w_pad + (x2 + 1);
        let c = (y2 + 1) * self.w_pad + x1;
        let d = (y2 + 1) * self.w_pad + (x2 + 1);

        let s = (self.sum[d] + self.sum[a]) - (self.sum[b] + self.sum[c]);
        let n = (self.cnt[d] + self.cnt[a]).saturating_sub(self.cnt[b] + self.cnt[c]);
        (s, n)
    }
}

pub fn find_top_anomalies(
    ndvi_grid: &[f64],
    total_w: usize,
    total_h: usize,
    well_cx: f32,
    well_cy: f32,
    pixel_size_m: f64,
    top_n: usize,
) -> Vec<AnomalyCandidate> {
    let sat = IntegralImage::new(ndvi_grid, total_w, total_h);

    // Размеры скользящего окна в пикселях (1 px = 10 м):
    let r_core = 9_usize; // Ядро ~180x180 m
    let r_buf = 12_usize; // Ободок 240x240 m
    let r_bg = 24_usize; // Внешний фон ~480x480 m
    let step = 6_usize; // Шаг сканирования = 60 m

    let mut raw_candidates = Vec::new();
    let mut all_deltas = Vec::new();

    for cy in (r_bg..(total_h - r_bg)).step_by(step) {
        for cx in (r_bg..(total_w - r_bg)).step_by(step) {
            let (core_sum, core_cnt) =
                sat.query(cx - r_core, cy - r_core, cx + r_core, cy + r_core);
            let (buf_sum, buf_cnt) = sat.query(cx - r_buf, cy - r_buf, cx + r_buf, cy + r_buf);
            let (bg_sum, bg_cnt) = sat.query(cx - r_bg, cy - r_bg, cx + r_bg, cy + r_bg);

            let ring_sum = bg_sum - buf_sum;
            let ring_cnt = bg_cnt - buf_cnt;

            // Отбрасываем окна, где много битых пикселей (NoData)
            if core_cnt < 300 || ring_cnt < 1400 {
                continue;
            }

            let core_mean = core_sum / core_cnt as f64;
            let ring_mean = ring_sum / ring_cnt as f64;

            // Отсекаем плотный прибрежный лес и орошаемые поля (круги формируются в сухой саванне)
            if ring_mean > 0.52 {
                continue;
            }

            let delta = core_mean - ring_mean;

            all_deltas.push(delta);

            // ФИЛЬТР «АНТИ-ЗАБОР»: Проверяем радиальную симметрию по 4 секторам (С, Ю, З, В)
            let (n_s, n_c) = sat.query(cx - r_core, cy - r_bg, cx + r_core, cy - r_buf);
            let (s_s, s_c) = sat.query(cx - r_core, cy + r_buf, cx + r_core, cy + r_bg);
            let (w_s, w_c) = sat.query(cx - r_bg, cy - r_core, cx - r_buf, cy + r_core);
            let (e_s, e_c) = sat.query(cx + r_buf, cy - r_core, cx + r_bg, cy + r_core);

            if n_c < 150 || s_c < 150 || w_c < 150 || e_c < 150 {
                continue;
            }

            let sectors = [
                n_s / n_c as f64,
                s_s / s_c as f64,
                w_s / w_c as f64,
                e_s / e_c as f64,
            ];

            let max_sec = sectors.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            let min_sec = sectors.iter().cloned().fold(f64::INFINITY, f64::min);
            let symmetry_diff = max_sec - min_sec;

            // Отсекаем дороги, ручьи и прямые границы пастбищ (где одна сторона лес, а другая пустошь)
            if symmetry_diff > 0.075 {
                continue;
            }

            let dist_m = (((cx as f32 - well_cx).powi(2) + (cy as f32 - well_cy).powi(2)).sqrt()
                * pixel_size_m as f32) as f64;

            raw_candidates.push((cx, cy, delta, core_mean, ring_mean, symmetry_diff, dist_m));
        }
    }

    // Считаем среднее и стандартное отклонение по всей карте для расчета Z-Score
    let mean_delta = all_deltas.iter().sum::<f64>() / all_deltas.len() as f64;
    let variance = all_deltas
        .iter()
        .map(|d| (d - mean_delta).powi(2))
        .sum::<f64>()
        / all_deltas.len() as f64;
    let std_delta = variance.sqrt();

    // Сортируем по силе контраста (по модулю), чтобы ловить и лысые центры, и зеленые ядра в светлом кольце
    // raw_candidates.sort_by(|a, b| b.2.abs().partial_cmp(&a.2.abs()).unwrap());
    raw_candidates.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap());

    // Фильтруем дубликаты (Non-Maximum Suppression): берем точки не ближе 1.5 км друг к другу
    let mut top_anomalies: Vec<AnomalyCandidate> = Vec::new();
    for (cx, cy, delta, core_ndvi, bg_ndvi, symmetry_diff, dist_to_well_m) in raw_candidates {
        let too_close = top_anomalies.iter().any(|picked| {
            let d = (((cx as f64 - picked.cx as f64).powi(2)
                + (cy as f64 - picked.cy as f64).powi(2))
            .sqrt())
                * pixel_size_m;
            d < 350.0
        });

        if !too_close {
            let z_score = (delta - mean_delta) / std_delta;
            top_anomalies.push(AnomalyCandidate {
                cx,
                cy,
                delta_ndvi: delta,
                core_ndvi,
                bg_ndvi,
                z_score,
                symmetry_diff,
                dist_to_well_m,
            });

            if top_anomalies.len() == top_n {
                break;
            }
        }
    }

    top_anomalies
}
