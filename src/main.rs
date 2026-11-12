mod reqwest_range_reader;

use polars::prelude::*;
use proj4rs::Proj;
use reqwest_range_reader::RemoteCog;
use serde_json::Value;
use stac_client::{Client as ApiClient, SearchParams};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("1. Ищем чистую сцену над месторождением Буракебугу (Мали)...");
    let stac_url = "https://planetarycomputer.microsoft.com/api/stac/v1";
    let client = ApiClient::new(stac_url)?;

    let target_lon = -5.130_f64;
    let target_lat = 13.002_f64;

    let params = SearchParams {
        collections: Some(vec!["sentinel-2-l2a".to_string()]),
        bbox: Some(vec![-5.18, 12.95, -5.08, 13.05]),
        datetime: Some("2024-01-01T00:00:00Z/2024-04-01T00:00:00Z".to_string()),
        limit: Some(15),
        ..Default::default()
    };

    let results = client.search(&params).await?;
    let best_item = results
        .features
        .into_iter()
        .filter(|item| {
            let nodata = item
                .properties
                .get("s2:nodata_pixel_percentage")
                .and_then(|v| v.as_f64())
                .unwrap_or(100.0);
            nodata < 10.0
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
        .ok_or("Не найдено полных сцен без NoData")?;

    println!("Выбрана сцена: {}", best_item.id);

    // 2. Переводим градусы (lon, lat) в метры UTM Zone 30N
    let from_wgs84 = Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs")?;
    let to_utm30n = Proj::from_proj_string("+proj=utm +zone=30 +datum=WGS84 +units=m +no_defs")?;
    let mut point = (target_lon.to_radians(), target_lat.to_radians(), 0.0);
    proj4rs::transform::transform(&from_wgs84, &to_utm30n, &mut point)?;
    let (utm_x, utm_y) = (point.0, point.1);

    let b04_asset = best_item.assets.get("B04").ok_or("Нет канала B04")?;
    let b08_asset = best_item.assets.get("B08").ok_or("Нет канала B08")?;

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

    let b04_url = b04_signed["href"].as_str().ok_or("Не подписался B04")?;
    let b08_url = b08_signed["href"].as_str().ok_or("Не подписался B08")?;

    // 4. Открываем заголовки COG и узнаём реальные размеры тайла
    let (b04_cog_res, b08_cog_res) = tokio::join!(
        RemoteCog::open(&http, b04_url),
        RemoteCog::open(&http, b08_url),
    );
    let b04_cog = b04_cog_res?;
    let b08_cog = b08_cog_res?;

    let tw = b04_cog.tile_width;
    let th = b04_cog.tile_height;
    let center_tx = global_px / tw;
    let center_ty = global_py / th;

    println!(
        "2. Качаем сетку 3x3 тайла вокруг [{}, {}] параллельно по Range...",
        center_tx, center_ty
    );

    // 5. Параллельно качаем 3x3 тайла для обоих каналов
    let (grid04_res, grid08_res) = tokio::join!(
        b04_cog.fetch_tile_grid_u16(center_tx, center_ty, 1),
        b08_cog.fetch_tile_grid_u16(center_tx, center_ty, 1),
    );
    let (b04_pixels, total_w, total_h, off_x, off_y) = grid04_res?;
    let (b08_pixels, _, _, _, _) = grid08_res?;

    // Новые координаты скважины внутри склеенного холста 1536x1536
    let stitched_cx = ((global_px % tw) + off_x) as f32;
    let stitched_cy = ((global_py % th) + off_y) as f32;

    println!(
        "3. Холст склеен: {}x{} px (15.3x15.3 км), центр скважин: ({}, {})",
        total_w, total_h, stitched_cx, stitched_cy
    );

    // 6. Размечаем зоны и применяем физическую калибровку Sentinel-2 (ESA Offset -1000)
    let core_r = (800.0 / res_x.abs()) as f32; // Круг 800 м вокруг скважин
    let bg_inner_r = (1600.0 / res_x.abs()) as f32; // Внутренний радиус фонового кольца (1.6 км)
    let bg_outer_r = (3500.0 / res_x.abs()) as f32; // Внешний радиус фонового кольца (3.5 км)

    let n = total_w * total_h;
    let mut zone = Vec::with_capacity(n);
    let mut red_refl = Vec::with_capacity(n);
    let mut nir_refl = Vec::with_capacity(n);

    for idx in 0..n {
        let raw_b04 = b04_pixels[idx];
        let raw_b08 = b08_pixels[idx];

        // Отсекаем пустые пиксели (NoData = 0)
        if raw_b04 == 0 || raw_b08 == 0 {
            continue;
        }

        let x = (idx % total_w) as f32;
        let y = (idx / total_w) as f32;
        let dist = ((x - stitched_cx).powi(2) + (y - stitched_cy).powi(2)).sqrt();

        let label = if dist <= core_r {
            "H2_Wells_0_800m"
        } else if dist >= bg_inner_r && dist <= bg_outer_r {
            "Savanna_Ring_1.6_3.5km"
        } else {
            continue; // Всё остальное не тащим в DataFrame — экономим память и CPU
        };

        // Убираем ESA Radio Offset (+1000) и переводим в коэффициент отражения (0.0 .. 1.0)
        let r = ((raw_b04 as f64 - 1000.0) / 10000.0).max(0.0);
        let nir = ((raw_b08 as f64 - 1000.0) / 10000.0).max(0.0);

        zone.push(label);
        red_refl.push(r);
        nir_refl.push(nir);
    }

    let df = df!(
        "zone" => &zone,
        "red_refl" => &red_refl,
        "nir_refl" => &nir_refl,
    )?;

    // 7. Считаем физически точный NDVI в Polars
    let analysis = df
        .lazy()
        .with_columns([((col("nir_refl") - col("red_refl"))
            / (col("nir_refl") + col("red_refl") + lit(1e-6)))
        .alias("ndvi")])
        .group_by([col("zone")])
        .agg([
            col("ndvi").count().alias("pixels_10m"),
            col("ndvi").mean().alias("mean_ndvi"),
            col("ndvi")
                .quantile(lit(0.1), QuantileMethod::Linear)
                .alias("p10_ndvi_stress"),
            col("nir_refl").mean().alias("mean_nir_refl"),
        ])
        .collect()?;

    println!("\n=== Эталонный физический NDVI (ESA Offset -1000, сетка 3x3 без обрезки) ===");
    println!("{}", analysis);

    Ok(())
}
