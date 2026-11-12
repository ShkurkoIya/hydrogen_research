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

    /// Вычитывает конкретный тайл (tile_x, tile_y) и декодирует его в u16
    pub async fn fetch_tile_u16(
        &self,
        tile_x: usize,
        tile_y: usize,
    ) -> Result<Vec<u16>, Box<dyn std::error::Error>> {
        let ifd = self.tiff.ifds().first().ok_or("Нет базового IFD")?;
        let tile = ifd.fetch_tile(tile_x, tile_y, self.reader.as_ref()).await?;
        let registry = DecoderRegistry::default();
        let decoded = tile.decode(&registry)?;

        /// Читывает один тайл (tile_x, tile_y) и декодирует его в u16
        let raw_bytes: &[u8] = decoded.data().as_ref();
        let pixels: Vec<u16> = raw_bytes
            .array_windows::<2>()
            .step_by(2)
            .map(|&bytes| u16::from_le_bytes(bytes))
            .collect();

        Ok(pixels)
    }

    /// Скачивает сетку (2*radius + 1) x (2*radius + 1) тайлов вокруг центрального и склеивает их в единый растр.
    /// Возвращает: (склеенный_буфер, общая_ширина, общая_высота, сдвиг_x, сдвиг_y)
    pub async fn fetch_tile_grid_u16(
        &self,
        center_tx: usize,
        center_ty: usize,
        radius: isize,
    ) -> Result<(Vec<u16>, usize, usize, usize, usize), Box<dyn std::error::Error>> {
        let start_tx = (center_tx as isize - radius).max(0) as usize;
        let start_ty = (center_ty as isize - radius).max(0) as usize;
        let end_tx = (center_tx as isize + radius) as usize;
        let end_ty = (center_ty as isize + radius) as usize;

        let cols = end_tx - start_tx + 1;
        let rows = end_ty - start_ty + 1;
        let total_w = cols * self.tile_width;
        let total_h = rows * self.tile_height;

        // Готовим параллельные задачи на скачивание всех тайлов сетки
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

        // Раскладываем каждый скачанный тайл на своё место в большом холсте
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
}
