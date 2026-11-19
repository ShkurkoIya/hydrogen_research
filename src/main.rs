pub mod blind_search;
pub mod dem_search;
pub mod geojson_export;
pub mod math_ndvi;
pub mod reqwest_range_reader;

use polars::prelude::*;
use proj4rs::Proj;
use reqwest_range_reader::RemoteCog;
use serde_json::Value;
use stac_client::{Client as ApiClient, SearchParams};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    unsafe { std::env::set_var("POLARS_FMT_MAX_ROWS", "30") }

    let stac_url = "https://planetarycomputer.microsoft.com/api/stac/v1";
    let client = ApiClient::new(stac_url)?;

    // 1. Координаты эталонного водородного круга в бассейне Сан-Франсиску (Бразилия)
    let target_lon = -45.185_f64;
    let target_lat = -16.365_f64;

    // Квадрат ~11x11 км вокруг точки (отступ ±0.05 градуса)
    let params = SearchParams {
        collections: Some(vec!["sentinel-2-l2a".to_string()]),
        bbox: Some(vec![-45.15, -16.49, -45.04, -16.38]),
        // В Бразилии сухой безоблачный сезон — с июля по сентябрь!
        datetime: Some("2022-07-01T00:00:00Z/2022-09-30T00:00:00Z".to_string()),
        limit: Some(50),
        ..Default::default()
    };

    let results = client.search(&params).await?;
    println!("Найдено всего сцен в STAC: {}", results.features.len());

    let best_item = results
        .features
        .into_iter()
        .filter(|item| {
            let nodata = item
                .properties
                .get("s2:nodata_pixel_percentage")
                .and_then(|v| v.as_f64())
                .unwrap_or(100.0);
            nodata < 45.0
        })
        .min_by(|a, b| {
            let ca = a
                .properties
                .get("eo:cloud_cover")
                .and_then(|v| v.as_f64())
                .unwrap_or(100.0);
            let cb = b
                .properties
                .get("eo:cloud_cover")
                .and_then(|v| v.as_f64())
                .unwrap_or(100.0);
            ca.partial_cmp(&cb).unwrap()
        })
        .ok_or("Не найдено подходящих сцен")?;

    println!("Выбрана сцена: {}", best_item.id);

    // 2. Переводим градусы (lon, lat) в метры UTM Zone 30N
    let from_wgs84 = Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs")?;
    let to_utm30n =
        Proj::from_proj_string("+proj=utm +zone=23 +south +datum=WGS84 +units=m +no_defs")?;
    let mut point = (target_lon.to_radians(), target_lat.to_radians(), 0.0);
    proj4rs::transform::transform(&from_wgs84, &to_utm30n, &mut point)?;
    let (utm_x, utm_y) = (point.0, point.1);

    let b04_asset = best_item.assets.get("B04").ok_or("Нет канала B04")?;
    let b08_asset = best_item.assets.get("B08").ok_or("Нет канала B08")?;
    let b11_asset = best_item.assets.get("B11").ok_or("Нет канала B11")?;

    let transform = b04_asset
        .extra
        .get("proj:transform")
        .and_then(|v| v.as_array())
        .ok_or("Нет proj:transform")?;

    let res_x = transform.first().and_then(|v| v.as_f64()).unwrap_or(10.0);
    let origin_x = transform.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let res_y = transform.get(4).and_then(|v| v.as_f64()).unwrap_or(-10.0);
    let origin_y = transform.get(5).and_then(|v| v.as_f64()).unwrap_or(0.0);

    let global_px = ((utm_x - origin_x) / res_x) as usize;
    let global_py = ((utm_y - origin_y) / res_y) as usize;

    // 3. Подписываем ссылки на COG B04 и B08
    let http = reqwest::Client::new();
    let sign_url = "https://planetarycomputer.microsoft.com/api/sas/v1/sign";

    let b04_signed: Value = http
        .get(sign_url)
        .query(&[("href", b04_asset.href.as_str())])
        .send()
        .await?
        .json()
        .await?;
    let b08_signed: Value = http
        .get(sign_url)
        .query(&[("href", b08_asset.href.as_str())])
        .send()
        .await?
        .json()
        .await?;

    let b11_signed: Value = http
        .get(sign_url)
        .query(&[("href", b11_asset.href.as_str())])
        .send()
        .await?
        .json()
        .await?;

    let b04_url = b04_signed["href"].as_str().ok_or("Не подписался B04")?;
    let b08_url = b08_signed["href"].as_str().ok_or("Не подписался B08")?;
    let b11_url = b11_signed["href"].as_str().ok_or("Не подписалсяся B11")?;

    // 4. Открываем заголовки COG и узнаём реальные размеры тайла
    let (b04_cog_res, b08_cog_res, b11_cog_res) = tokio::join!(
        RemoteCog::open(&http, b04_url),
        RemoteCog::open(&http, b08_url),
        RemoteCog::open(&http, b11_url),
    );
    let b04_cog = b04_cog_res?;
    let b08_cog = b08_cog_res?;
    let b11_cog = b11_cog_res?;

    let tw = b04_cog.tile_width;
    let th = b04_cog.tile_height;
    let center_tx = global_px / tw;
    let center_ty = global_py / th;

    let global_px_20m = global_px / 2;
    let global_py_20m = global_py / 2;
    let center_tx_11 = global_px_20m / b11_cog.tile_width;
    let center_ty_11 = global_py_20m / b11_cog.tile_height;

    println!(
        "2. Качаем сетку 3x3 тайла вокруг [{}, {}] параллельно по Range...",
        center_tx, center_ty
    );

    // 5. Параллельно качаем 3x3 тайла для обоих каналов
    let (grid04_res, grid08_res, grid11_res) = tokio::join!(
        b04_cog.fetch_tile_grid_u16(center_tx, center_ty, 1),
        b08_cog.fetch_tile_grid_u16(center_tx, center_ty, 1),
        b11_cog.fetch_tile_grid_u16(center_tx_11, center_ty_11, 1),
    );
    let (b04_pixels, total_w, total_h, off_x, off_y) = grid04_res?;
    let (b08_pixels, w08, h08, _, _) = grid08_res?;
    let (b11_raw, w11, h11, off_x11, off_y11) = grid11_res?;

    let start_gx_10 = (center_tx * tw) - off_x;
    let start_gy_10 = (center_ty * th) - off_y;
    let start_gx_20 = (center_tx_11 * b11_cog.tile_width) - off_x11;
    let start_gy_20 = (center_ty_11 * b11_cog.tile_height) - off_y11;

    let mut b11_pixels = vec![0_u16; total_w * total_h];
    for y in 0..total_h {
        let gy_20 = (start_gy_10 + y) / 2;
        if gy_20 >= start_gy_20 && gy_20 < start_gy_20 + h11 {
            let ly_20 = gy_20 - start_gy_20;
            for x in 0..total_w {
                let gx_20 = (start_gx_10 + x) / 2;
                if gx_20 >= start_gx_20 && gx_20 < start_gx_20 + w11 {
                    let lx_20 = gx_20 - start_gx_20;
                    b11_pixels[y * total_w + x] = b11_raw[ly_20 * w11 + lx_20];
                }
            }
        }
    }

    if b04_pixels.len() != b08_pixels.len() {
        return Err(format!(
            "Несовпадение размеров каналов: B04 ({}x{} = {}) != B08 ({}x{} = {}). Попробуйте другую орбиту.",
            total_w, total_h, b04_pixels.len(), w08, h08, b08_pixels.len()
        ).into());
    }

    // Новые координаты скважины внутри склеенного холста 1536x1536
    let stitched_cx = ((global_px % tw) + off_x) as f32;
    let stitched_cy = ((global_py % th) + off_y) as f32;

    println!(
        "3. Холст склеен: {}x{} px (15.3x15.3 км), центр скважин: ({}, {})",
        total_w, total_h, stitched_cx, stitched_cy
    );

    // 6. Размечаем зоны и применяем физическую калибровку Sentinel-2 (ESA Offset -1000)
    let control_analysis = math_ndvi::analyze_control_zones(
        &b04_pixels,
        &b08_pixels,
        total_w,
        stitched_cx,
        stitched_cy,
        res_x.abs(),
    )?;
    println!("\n=== 4. Эталонный физический NDVI (ESA Offset -1000) ===");
    println!("{}", control_analysis);

    // 7. Считаем общую 2D-матрицу NDVI для дальнейших алгоритмов
    let ndvi_grid = math_ndvi::compute_ndvi_grid(&b04_pixels, &b08_pixels, &b11_pixels);

    // 8. ГРАДИЕНТ: Радиальный профиль (кольца по 200 м до 3.6 км)
    let profile = math_ndvi::build_radial_profile(
        &ndvi_grid,
        total_w,
        stitched_cx,
        stitched_cy,
        res_x.abs(),
        3600.0,
        200,
    )?;
    println!("\n=== 5. Радиальный профиль NDVI от эпицентра Буракебугу ===");
    println!("{}", profile);

    // 9. СЛЕПОЙ ПОИСК: Сканируем весь холст 15.3x15.3 км скользящим окном и рельеф

    println!(
        "\n=== 6. Загрузка цифровой модели рельефа (Copernicus DEM 30m) и поиск просадок... ==="
    );
    let canvas_origin_gx = (center_tx - 1) * tw;
    let canvas_origin_gy = (center_ty - 1) * th;

    let search_bbox = vec![-45.25, -16.43, -45.12, -16.30];
    let dem_grid = dem_search::fetch_aligned_dem(
        &client,
        &http,
        &search_bbox,
        origin_x,
        origin_y,
        res_x,
        res_y,
        canvas_origin_gx,
        canvas_origin_gy,
        total_w,
        total_h,
    )
    .await?;

    let top_anomalies = dem_search::find_geological_anomalies(
        &ndvi_grid,
        &dem_grid,
        total_w,
        total_h,
        stitched_cx,
        stitched_cy,
        res_x.abs(),
        5,
    );

    println!("\n=== 7. ТОП-5 ГЕОЛОГИЧЕСКИХ АНОМАЛИЙ (ЗАМКНУТАЯ ПРОСАДКА РЕЛЬЕФА + NDVI) ===");
    println!(
        "{:<5} | {:<10} | {:<10} | {:<10} | {:<11} | {:<9}",
        "Ранг", "Core NDVI", "Фон NDVI", "ΔNDVI", "Просадка(м)", "Z-Score"
    );
    println!("{}", "-".repeat(72));

    for (i, anom) in top_anomalies.iter().enumerate() {
        println!(
            "#{:<4} | {:<10.4} | {:<10.4} | {:<+10.4} | {:<11.2} | {:<+9.2}",
            i + 1,
            anom.core_ndvi,
            anom.bg_ndvi,
            anom.delta_ndvi,
            anom.symmetry_diff,
            anom.z_score
        );
    }

    // let top_anomalies = blind_search::find_top_anomalies(
    //     &ndvi_grid,
    //     total_w,
    //     total_h,
    //     stitched_cx,
    //     stitched_cy,
    //     res_x.abs(),
    //     5,
    // );

    println!("\n=== 6. ТОП-5 НЕЗАВИСИМЫХ АНОМАЛИЙ (СЛЕПОЙ ПОИСК) ===");
    println!(
        "{:<5} | {:<10} | {:<10} | {:<10} | {:<9} | {:<18}",
        "Ранг", "Core NDVI", "Фон NDVI", "ΔNDVI", "Z-Score", "До скважины (м)"
    );
    println!("{}", "-".repeat(75));

    for (i, anom) in top_anomalies.iter().enumerate() {
        println!(
            "#{:<4} | {:<10.4} | {:<10.4} | {:<+10.4} | {:<+9.2} | {:<18.0}",
            i + 1,
            anom.core_ndvi,
            anom.bg_ndvi,
            anom.delta_ndvi,
            anom.z_score,
            anom.dist_to_well_m
        );
    }

    // 10. ВИЗУАЛИЗАЦИЯ: Сохраняем всё в GeoJSON для просмотра поверх спутника
    let geo_meta = geojson_export::GridGeoMeta {
        origin_x,
        origin_y,
        res_x,
        res_y,
        canvas_origin_gx: (center_tx - 1) * tw,
        canvas_origin_gy: (center_ty - 1) * th,
        total_w,
        total_h,
    };

    geojson_export::export_anomalies_to_geojson(
        &top_anomalies,
        &geo_meta,
        target_lon,
        target_lat,
        "h2_anomalies.geojson",
    )?;

    println!("\n✅ Готово! Файл 'h2_anomalies.geojson' сохранен.");

    Ok(())
}
