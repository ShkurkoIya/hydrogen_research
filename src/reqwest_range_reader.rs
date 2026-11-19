use async_tiff::TIFF;
use async_tiff::decoder::DecoderRegistry;
use async_tiff::error::AsyncTiffResult;
use async_tiff::metadata::TiffMetadataReader;
use async_tiff::metadata::cache::ReadaheadMetadataCache;
use async_tiff::reader::AsyncFileReader;
use async_trait::async_trait;
use bytes::Bytes;
use futures::future::try_join_all;
use std::ops::Range;
use std::sync::Arc;

#[derive(Clone, Debug)]
struct ReqwestRangeReader {
    client: reqwest::Client,
    url: String,
}

#[async_trait]
impl AsyncFileReader for ReqwestRangeReader {
    async fn get_bytes(&self, range: Range<u64>) -> AsyncTiffResult<Bytes> {
        let range_header = format!("bytes={}-{}", range.start, range.end - 1);
        let resp = self
            .client
            .get(&self.url)
            .header(reqwest::header::RANGE, range_header)
            .send()
            .await
            .map_err(|e| async_tiff::error::AsyncTiffError::General(e.to_string()))?;

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| async_tiff::error::AsyncTiffError::General(e.to_string()))?;

        Ok(bytes)
    }
}

pub struct RemoteCog {
    reader: Arc<dyn AsyncFileReader>,
    tiff: TIFF,
    pub tile_width: usize,
    pub tile_height: usize,
}

impl RemoteCog {
    /// Открывает COG по сети, вычитывая только заголовок с метаданными и размерами тайлов
    pub async fn open(
        http: &reqwest::Client,
        signed_url: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let reader: Arc<dyn AsyncFileReader> = Arc::new(ReqwestRangeReader {
            client: http.clone(),
            url: signed_url.to_string(),
        });

        let cache = ReadaheadMetadataCache::new(reader.clone());
        let mut meta = TiffMetadataReader::try_open(&cache).await?;
        let idfs = meta.read_all_ifds(&cache).await?;

        let tiff = TIFF::new(idfs, meta.endianness());
        let ifd = tiff.ifds().first().ok_or("В COG отсутствуют IFD")?;

        let tile_width = ifd
            .tile_width()
            .ok_or("TIFF не является тайловым (нет tile_width)")? as usize;
        let tile_height =
            ifd.tile_height()
                .ok_or("TIFF не является тайловым (нет tile_height)")? as usize;

        Ok(Self {
            reader,
            tiff,
            tile_width,
            tile_height,
        })
    }

    /// Вычитывает конкретный тайл (tile_x, tile_y) и декодирует его в u16.
    /// Если тайл на краю снимка пустой или обрезан — безопасно возвращает нули (NoData).
    pub async fn fetch_tile_u16(
        &self,
        tile_x: usize,
        tile_y: usize,
    ) -> Result<Vec<u16>, Box<dyn std::error::Error>> {
        let expected_pixels = self.tile_width * self.tile_height;
        let ifd = self.tiff.ifds().first().ok_or("Нет базового IFD")?;

        let tile = match ifd.fetch_tile(tile_x, tile_y, self.reader.as_ref()).await {
            Ok(t) => t,
            Err(_) => return Ok(vec![0_u16; expected_pixels]),
        };

        let registry = DecoderRegistry::default();
        let decoded = match tile.decode(&registry) {
            Ok(d) => d,
            Err(e) => {
                // Если граничный тайл обрезан или пуст в COG — заполняем его NoData (нулями)
                eprintln!(
                    "⚠️ Тайл [{}, {}] пропущен как граничный/пустой ({})",
                    tile_x, tile_y, e
                );
                return Ok(vec![0_u16; expected_pixels]);
            }
        };

        let raw_bytes: &[u8] = decoded.data().as_ref();
        let mut pixels: Vec<u16> = raw_bytes
            .array_windows::<2>()
            .step_by(2)
            .map(|&bytes| u16::from_le_bytes(bytes))
            .collect();

        // Гарантируем, что длина вектора строго равна tile_width * tile_height
        pixels.resize(expected_pixels, 0_u16);
        Ok(pixels)
    }

    /// Скачивает сетку (2*radius + 1) x (2*radius + 1) тайлов вокруг центрального и склеивает их в единый растр.
    pub async fn fetch_tile_grid_u16(
        &self,
        center_tx: usize,
        center_ty: usize,
        radius: isize,
    ) -> Result<(Vec<u16>, usize, usize, usize, usize), Box<dyn std::error::Error>> {
        let ifd = self.tiff.ifds().first().ok_or("Нет базового IFD")?;
        let img_w = ifd.image_width() as usize;
        let img_h = ifd.image_height() as usize;

        let max_tx = img_w.div_ceil(self.tile_width).saturating_sub(1);
        let max_ty = img_h.div_ceil(self.tile_height).saturating_sub(1);

        let start_tx = (center_tx as isize - radius).max(0) as usize;
        let start_ty = (center_ty as isize - radius).max(0) as usize;
        let end_tx = ((center_tx as isize + radius) as usize).min(max_tx);
        let end_ty = ((center_ty as isize + radius) as usize).min(max_ty);

        let cols = end_tx - start_tx + 1;
        let rows = end_ty - start_ty + 1;

        let total_w = cols * self.tile_width;
        let total_h = rows * self.tile_height;

        let mut tasks = Vec::with_capacity(cols * rows);
        for ty in start_ty..=end_ty {
            for tx in start_tx..=end_tx {
                tasks.push(async move {
                    let data = self.fetch_tile_u16(tx, ty).await?;
                    Ok::<(usize, usize, Vec<u16>), Box<dyn std::error::Error>>((tx, ty, data))
                });
            }
        }

        let tiles = try_join_all(tasks).await?;
        let mut stitched = vec![0_u16; total_w * total_h];

        for (tx, ty, data) in tiles {
            let col_idx = tx - start_tx;
            let row_idx = ty - start_ty;
            let base_x = col_idx * self.tile_width;
            let base_y = row_idx * self.tile_height;

            for y in 0..self.tile_height {
                let src_start = y * self.tile_width;
                let src_end = src_start + self.tile_width;
                let dst_start = (base_y + y) * total_w + base_x;
                stitched[dst_start..dst_start + self.tile_width]
                    .copy_from_slice(&data[src_start..src_end]);
            }
        }

        let offset_x = (center_tx - start_tx) * self.tile_width;
        let offset_y = (center_ty - start_ty) * self.tile_height;

        Ok((stitched, total_w, total_h, offset_x, offset_y))
    }

    /// Вычитывает один тайл высот (DEM) и декодирует его в f32 (метры над уровнем моря)
    pub async fn fetch_tile_f32(
        &self,
        tile_x: usize,
        tile_y: usize,
    ) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
        let expected_pixels = self.tile_width * self.tile_height;
        let ifd = self.tiff.ifds().first().ok_or("Нет базового IFD")?;

        let tile = match ifd.fetch_tile(tile_x, tile_y, self.reader.as_ref()).await {
            Ok(t) => t,
            Err(_) => return Ok(vec![f32::NAN; expected_pixels]),
        };

        let registry = DecoderRegistry::default();
        let decoded = match tile.decode(&registry) {
            Ok(d) => d,
            Err(_) => return Ok(vec![f32::NAN; expected_pixels]),
        };

        let raw_bytes: &[u8] = decoded.data().as_ref();
        let mut pixels: Vec<f32> = raw_bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();

        pixels.resize(expected_pixels, f32::NAN);
        Ok(pixels)
    }

    /// Скачивает сетку (2*radius + 1) x (2*radius + 1) тайлов высот DEM
    pub async fn fetch_tile_grid_f32(
        &self,
        center_tx: usize,
        center_ty: usize,
        radius: isize,
    ) -> Result<(Vec<f32>, usize, usize, usize, usize), Box<dyn std::error::Error>> {
        let ifd = self.tiff.ifds().first().ok_or("Нет базового IFD")?;
        let img_w = ifd.image_width() as usize;
        let img_h = ifd.image_height() as usize;

        let max_tx = img_w.div_ceil(self.tile_width).saturating_sub(1);
        let max_ty = img_h.div_ceil(self.tile_height).saturating_sub(1);

        let start_tx = (center_tx as isize - radius).max(0) as usize;
        let start_ty = (center_ty as isize - radius).max(0) as usize;
        let end_tx = ((center_tx as isize + radius) as usize).min(max_tx);
        let end_ty = ((center_ty as isize + radius) as usize).min(max_ty);

        let cols = end_tx - start_tx + 1;
        let rows = end_ty - start_ty + 1;
        let total_w = cols * self.tile_width;
        let total_h = rows * self.tile_height;

        let mut tasks = Vec::with_capacity(cols * rows);
        for ty in start_ty..=end_ty {
            for tx in start_tx..=end_tx {
                tasks.push(async move {
                    let data = self.fetch_tile_f32(tx, ty).await?;
                    Ok::<(usize, usize, Vec<f32>), Box<dyn std::error::Error>>((tx, ty, data))
                });
            }
        }

        let tiles = try_join_all(tasks).await?;
        let mut stitched = vec![f32::NAN; total_w * total_h];

        for (tx, ty, data) in tiles {
            let col_idx = tx - start_tx;
            let row_idx = ty - start_ty;
            let base_x = col_idx * self.tile_width;
            let base_y = row_idx * self.tile_height;

            for y in 0..self.tile_height {
                let src_start = y * self.tile_width;
                let src_end = src_start + self.tile_width;
                let dst_start = (base_y + y) * total_w + base_x;
                stitched[dst_start..dst_start + self.tile_width]
                    .copy_from_slice(&data[src_start..src_end]);
            }
        }

        let offset_x = (center_tx - start_tx) * self.tile_width;
        let offset_y = (center_ty - start_ty) * self.tile_height;

        Ok((stitched, total_w, total_h, offset_x, offset_y))
    }

    pub fn image_dimensions(&self) -> Result<(usize, usize), Box<dyn std::error::Error>> {
        let ifd = self.tiff.ifds().first().ok_or("Нет базового IFD")?;
        Ok((ifd.image_width() as usize, ifd.image_height() as usize))
    }
}
