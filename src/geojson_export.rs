use crate::blind_search::AnomalyCandidate;
use proj4rs::Proj;
use serde_json::json;
use std::error::Error;

pub struct GridGeoMeta {
    pub origin_x: f64,
    pub origin_y: f64,
    pub res_x: f64,
    pub res_y: f64,
    pub canvas_origin_gx: usize,
    pub canvas_origin_gy: usize,
    pub total_w: usize,
    pub total_h: usize,
}

/// Экспортирует исследуемую область, найденные аномалии и эталонную точку в GeoJSON
pub fn export_anomalies_to_geojson(
    anomalies: &[AnomalyCandidate],
    meta: &GridGeoMeta,
    well_lon: f64,
    well_lat: f64,
    output_path: &str,
) -> Result<(), Box<dyn Error>> {
    let from_wgs84 = Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs")?;
    let to_utm23s =
        Proj::from_proj_string("+proj=utm +zone=23 +south +datum=WGS84 +units=m +no_defs")?;

    let mut features = Vec::new();

    // 0. Рисуем тонкую рамку всей исследуемой области (15.3x15.3 км) без заливки
    let min_utm_x = meta.origin_x + (meta.canvas_origin_gx as f64) * meta.res_x;
    let max_utm_x = meta.origin_x + ((meta.canvas_origin_gx + meta.total_w) as f64) * meta.res_x;
    let max_utm_y = meta.origin_y + (meta.canvas_origin_gy as f64) * meta.res_y;
    let min_utm_y = meta.origin_y + ((meta.canvas_origin_gy + meta.total_h) as f64) * meta.res_y;

    let boundary_utm = [
        (min_utm_x, min_utm_y),
        (max_utm_x, min_utm_y),
        (max_utm_x, max_utm_y),
        (min_utm_x, max_utm_y),
        (min_utm_x, min_utm_y),
    ];

    let mut boundary_coords = Vec::with_capacity(5);
    for (bx, by) in boundary_utm {
        let mut pt = (bx, by, 0.0);
        proj4rs::transform::transform(&to_utm23s, &from_wgs84, &mut pt)?;
        boundary_coords.push(json!([pt.0.to_degrees(), pt.1.to_degrees()]));
    }

    features.push(json!({
        "type": "Feature",
        "properties": {
            "name": "Граница исследуемой области (15.3x15.3 км)",
            "stroke": "#00e5ff",
            "stroke-width": 1.5,
            "stroke-opacity": 0.7,
            "fill-opacity": 0.0
        },
        "geometry": {
            "type": "Polygon",
            "coordinates": [boundary_coords]
        }
    }));

    // 1. Рисуем квадраты найденных аномалий (600x600 м под размер ведьминых кругов)
    for (i, anom) in anomalies.iter().enumerate() {
        let gx = (meta.canvas_origin_gx + anom.cx) as f64;
        let gy = (meta.canvas_origin_gy + anom.cy) as f64;
        let utm_px = meta.origin_x + gx * meta.res_x;
        let utm_py = meta.origin_y + gy * meta.res_y;

        let mut center_pt = (utm_px, utm_py, 0.0);
        proj4rs::transform::transform(&to_utm23s, &from_wgs84, &mut center_pt)?;
        let lon = center_pt.0.to_degrees();
        let lat = center_pt.1.to_degrees();

        let half_box_m = 150.0;
        let corners_utm = [
            (utm_px - half_box_m, utm_py - half_box_m),
            (utm_px + half_box_m, utm_py - half_box_m),
            (utm_px + half_box_m, utm_py + half_box_m),
            (utm_px - half_box_m, utm_py + half_box_m),
            (utm_px - half_box_m, utm_py - half_box_m),
        ];

        let mut ring_coords = Vec::with_capacity(5);
        for (cx_m, cy_m) in corners_utm {
            let mut pt = (cx_m, cy_m, 0.0);
            proj4rs::transform::transform(&to_utm23s, &from_wgs84, &mut pt)?;
            ring_coords.push(json!([pt.0.to_degrees(), pt.1.to_degrees()]));
        }

        let color = if i == 0 { "#ff0000" } else { "#ffaa00" };

        features.push(json!({
            "type": "Feature",
            "properties": {
                "rank": i + 1,
                "delta_ndvi": (anom.delta_ndvi * 10000.0).round() / 10000.0,
                "z_score": (anom.z_score * 100.0).round() / 100.0,
                "core_ndvi": (anom.core_ndvi * 10000.0).round() / 10000.0,
                "dist_to_center_m": anom.dist_to_well_m.round(),
                "center_lon": lon,
                "center_lat": lat,
                "stroke": color,
                "stroke-width": 2,
                "fill": color,
                "fill-opacity": 0.2
            },
            "geometry": {
                "type": "Polygon",
                "coordinates": [ring_coords]
            }
        }));
    }

    // 2. Добавляем центральную опорную точку поиска
    features.push(json!({
        "type": "Feature",
        "properties": {
            "name": "Опорный центр поиска",
            "marker-color": "#00ff00",
            "marker-symbol": "star"
        },
        "geometry": {
            "type": "Point",
            "coordinates": [well_lon, well_lat]
        }
    }));

    let geojson = json!({
        "type": "FeatureCollection",
        "features": features
    });

    std::fs::write(output_path, serde_json::to_string_pretty(&geojson)?)?;
    Ok(())
}
