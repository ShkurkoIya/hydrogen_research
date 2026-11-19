use crate::blind_search::AnomalyCandidate;
use crate::reqwest_range_reader::RemoteCog;
use proj4rs::Proj;
use serde_json::Value;
use stac_client::{Client as ApiClient, SearchParams};
use std::error::Error;

/// Скачивает рельеф Copernicus DEM 30m и привязывает высоты (в метрах) к нашей 10м сетке UTM
pub async fn fetch_aligned_dem(
    client: &ApiClient,
    http: &reqwest::Client,
    bbox: &[f64],
    origin_x: f64,
    origin_y: f64,
    res_x: f64,
    res_y: f64,
    canvas_origin_gx: usize,
    canvas_origin_gy: usize,
    total_w: usize,
    total_h: usize,
) -> Result<Vec<f64>, Box<dyn Error>> {
    let dem_params = SearchParams {
        collections: Some(vec!["cop-dem-glo-30".to_string()]),
        bbox: Some(bbox.to_vec()),
        limit: Some(5),
        ..Default::default()
    };

    let dem_results = client.search(&dem_params).await?;
    let dem_item = dem_results
        .features
        .into_iter()
        .next()
        .ok_or("Не найден тайл рельефа cop-dem-glo-30")?;

    let data_asset = dem_item.assets.get("data").ok_or("Нет ассета data в DEM")?;
    let dem_bbox = dem_item.bbox.as_ref().ok_or("Нет bbox у тайла DEM")?;

    let sign_url = "https://planetarycomputer.microsoft.com/api/sas/v1/sign";
    let signed: Value = http
        .get(sign_url)
        .query(&[("href", data_asset.href.as_str())])
        .send()
        .await?
        .json()
        .await?;
    let dem_url = signed["href"].as_str().ok_or("Не подписался DEM")?;

    let dem_cog = RemoteCog::open(http, dem_url).await?;

    // Вычисляем геопривязку напрямую из bbox тайла DEM и размеров растра
    let (img_w, img_h) = dem_cog.image_dimensions()?;
    let dem_orig_x = dem_bbox[0]; // min_lon (левый край)
    let dem_orig_y = dem_bbox[3]; // max_lat (верхний край)
    let dem_res_x = (dem_bbox[2] - dem_bbox[0]) / (img_w as f64);
    let dem_res_y = (dem_bbox[1] - dem_bbox[3]) / (img_h as f64); // отрицательный шаг на юг

    // Находим центральный тайл DEM в градусах
    let center_lon = (bbox[0] + bbox[2]) / 2.0;
    let center_lat = (bbox[1] + bbox[3]) / 2.0;
    let dem_gpx = ((center_lon - dem_orig_x) / dem_res_x).max(0.0) as usize;
    let dem_gpy = ((center_lat - dem_orig_y) / dem_res_y).max(0.0) as usize;

    let tx = dem_gpx / dem_cog.tile_width;
    let ty = dem_gpy / dem_cog.tile_height;

    let (dem_raw, dw, dh, doff_x, doff_y) = dem_cog.fetch_tile_grid_f32(tx, ty, 1).await?;
    let start_dgx = (tx * dem_cog.tile_width) - doff_x;
    let start_dgy = (ty * dem_cog.tile_height) - doff_y;

    let from_wgs84 = Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs")?;
    let to_utm23s =
        Proj::from_proj_string("+proj=utm +zone=23 +south +datum=WGS84 +units=m +no_defs")?;

    let mut aligned_dem = vec![f64::NAN; total_w * total_h];

    // Перепроецируем каждый пиксель нашей 10м сетки в DEM (с шагом 3x3 для мгновенной скорости)
    for y in (0..total_h).step_by(3) {
        let utm_y = origin_y + ((canvas_origin_gy + y) as f64) * res_y;
        for x in (0..total_w).step_by(3) {
            let utm_x = origin_x + ((canvas_origin_gx + x) as f64) * res_x;

            let mut pt = (utm_x, utm_y, 0.0);
            proj4rs::transform::transform(&to_utm23s, &from_wgs84, &mut pt)?;
            let lon = pt.0.to_degrees();
            let lat = pt.1.to_degrees();

            let gx = ((lon - dem_orig_x) / dem_res_x).round() as isize;
            let gy = ((lat - dem_orig_y) / dem_res_y).round() as isize;

            let lx = gx - start_dgx as isize;
            let ly = gy - start_dgy as isize;

            if lx >= 0 && lx < dw as isize && ly >= 0 && ly < dh as isize {
                let elev = dem_raw[(ly as usize) * dw + (lx as usize)] as f64;
                if !elev.is_nan() {
                    for dy in 0..3 {
                        for dx in 0..3 {
                            if y + dy < total_h && x + dx < total_w {
                                aligned_dem[(y + dy) * total_w + (x + dx)] = elev;
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(aligned_dem)
}

/// Комплексный поиск: совмещает спектральную аномалию NDVI + геологическую микропросадку рельефа
pub fn find_geological_anomalies(
    ndvi_grid: &[f64],
    dem_grid: &[f64],
    total_w: usize,
    total_h: usize,
    well_cx: f32,
    well_cy: f32,
    pixel_size_m: f64,
    top_n: usize,
) -> Vec<AnomalyCandidate> {
    let r_core = 12_usize; // Радиус ядра круга ~120 м (диаметр 240 м)
    let r_ring = 28_usize; // Внешний край кольца ~280 м (диаметр 560 м)
    let step = 6_usize; // Шаг сканирования 60 м

    let mut candidates = Vec::new();
    let mut all_scores = Vec::new();

    for cy in (r_ring..(total_h - r_ring)).step_by(step) {
        for cx in (r_ring..(total_w - r_ring)).step_by(step) {
            let center_elev = dem_grid[cy * total_w + cx];
            if center_elev.is_nan() {
                continue;
            }

            // 1. Проверяем РЕЛЬЕФ: центр должен быть локальной замкнутой воронкой (ниже всех 4 сторон)
            let elev_n = dem_grid[(cy - r_ring) * total_w + cx];
            let elev_s = dem_grid[(cy + r_ring) * total_w + cx];
            let elev_w = dem_grid[cy * total_w + (cx - r_ring)];
            let elev_e = dem_grid[cy * total_w + (cx + r_ring)];

            if elev_n.is_nan() || elev_s.is_nan() || elev_w.is_nan() || elev_e.is_nan() {
                continue;
            }

            // Глубина просадки относительно каждой из 4 сторон
            let drop_n = elev_n - center_elev;
            let drop_s = elev_s - center_elev;
            let drop_w = elev_w - center_elev;
            let drop_e = elev_e - center_elev;

            // ЗАМКНУТАЯ ВОРОНКА: все 4 края должны быть выше центра минимум на 0.4 метра!
            // Реки и склоны здесь отсекаются на 100%, так как у реки один край всегда ниже центра.
            let min_rim_drop = drop_n.min(drop_s).min(drop_w).min(drop_e);
            let mean_depression_m = (drop_n + drop_s + drop_w + drop_e) / 4.0;

            if min_rim_drop < 0.4 || mean_depression_m > 3.5 {
                continue;
            }

            // 2. Считаем СПЕКТР (NDVI) в ядре и в кольце вокруг этой воронки
            let mut core_sum = 0.0;
            let mut core_cnt = 0;
            let mut ring_sum = 0.0;
            let mut ring_cnt = 0;

            for dy in -(r_ring as isize)..=(r_ring as isize) {
                for dx in -(r_ring as isize)..=(r_ring as isize) {
                    let dist_sq = dx * dx + dy * dy;
                    if dist_sq > (r_ring * r_ring) as isize {
                        continue;
                    }
                    let val = ndvi_grid
                        [((cy as isize + dy) as usize) * total_w + ((cx as isize + dx) as usize)];
                    if val.is_nan() {
                        continue;
                    }
                    if dist_sq <= (r_core * r_core) as isize {
                        core_sum += val;
                        core_cnt += 1;
                    } else if dist_sq >= ((r_core + 4) * (r_core + 4)) as isize {
                        ring_sum += val;
                        ring_cnt += 1;
                    }
                }
            }

            if core_cnt < 250 || ring_cnt < 800 {
                continue;
            }

            let core_ndvi = core_sum / core_cnt as f64;
            let bg_ndvi = ring_sum / ring_cnt as f64;
            let delta_ndvi = core_ndvi - bg_ndvi;

            if core_ndvi > 0.55 || bg_ndvi > 0.45 {
                continue;
            }

            // Комбинированный геологический индекс: глубина воронки (м) * контраст растительности
            let geo_score = mean_depression_m * delta_ndvi.abs();
            all_scores.push(geo_score);

            let dist_m = (((cx as f32 - well_cx).powi(2) + (cy as f32 - well_cy).powi(2)).sqrt()
                * pixel_size_m as f32) as f64;

            candidates.push((
                cx,
                cy,
                delta_ndvi,
                core_ndvi,
                bg_ndvi,
                mean_depression_m,
                geo_score,
                dist_m,
            ));
        }
    }

    if all_scores.is_empty() {
        return Vec::new();
    }

    let mean_sc = all_scores.iter().sum::<f64>() / all_scores.len() as f64;
    let std_sc = (all_scores
        .iter()
        .map(|s| (s - mean_sc).powi(2))
        .sum::<f64>()
        / all_scores.len() as f64)
        .sqrt()
        .max(1e-6);

    // Сортируем по убыванию комплексного геологического скора
    candidates.sort_by(|a, b| b.6.partial_cmp(&a.6).unwrap());

    let mut top = Vec::new();
    for (cx, cy, delta_ndvi, core_ndvi, bg_ndvi, depression_m, geo_score, dist_to_well_m) in
        candidates
    {
        let too_close = top.iter().any(|p: &AnomalyCandidate| {
            let d = (((cx as f64 - p.cx as f64).powi(2) + (cy as f64 - p.cy as f64).powi(2))
                .sqrt())
                * pixel_size_m;
            d < 400.0
        });

        if !too_close {
            top.push(AnomalyCandidate {
                cx,
                cy,
                delta_ndvi,
                core_ndvi,
                bg_ndvi,
                z_score: (geo_score - mean_sc) / std_sc,
                symmetry_diff: depression_m, // В это поле теперь пишем реальную глубину просадки в метрах!
                dist_to_well_m,
            });
            if top.len() == top_n {
                break;
            }
        }
    }

    top
}
