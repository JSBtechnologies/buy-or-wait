//! Unlimited-OCR (vLLM) ingestion client (fleet/specs/ocr_vllm_pipeline.md work item A1).
//! Replaces the fixed-JSON-key Qwen/gemma image reads: this model transcribes every label
//! and value as printed (as text lines or HTML table rows), and `extract::ocr_parse` +
//! `extract::labels` do the deterministic mapping into `ImageFigures` -- never this module's
//! job. OCR runs once per image at ingestion and is cached; `extract::images`'s witness gate
//! reads the cache, never calls this client per-request.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use image::{DynamicImage, GenericImageView};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const OCR_BASE_URL_ENV: &str = "OCR_BASE_URL";
const OCR_MODEL_ENV: &str = "OCR_MODEL";
const OCR_API_KEY_ENV: &str = "OCR_API_KEY";
const DEFAULT_MODEL: &str = "baidu/Unlimited-OCR";
const DEFAULT_PROMPT: &str = "<image>document parsing.";

/// Page-split thresholds (fleet/specs/ocr_vllm_pipeline.md A1) -- fixed by the spec, not
/// user-configurable, but folded into `OcrConfig::config_hash` so a future change to these
/// constants still invalidates the cache.
const NEAR_BLACK_LUMA_MAX: u8 = 25;
const NEAR_BLACK_RUN_MIN_PX: u32 = 6;
const SLIVER_MAX_PX: u32 = 50;

#[derive(Debug, Clone)]
pub struct OcrConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub max_tokens: u32,
    pub temperature: f64,
    pub ngram_size: u32,
    pub window_size: u32,
    /// Per-page upscale factor before OCR (spec: 2x, Lanczos3), applied only when a page
    /// needs it (see `upscale_min_side_px`).
    pub page_scale: f64,
    /// Lead finding 2026-09-13 (bus #ocr): a full-resolution page upscaled 2x runs a much
    /// higher risk of a runaway reply (image_06 at 2x drops its Total in the GSTIN/CIN-footer
    /// runaway; 1x keeps it). A page whose shorter side is already at or above this many
    /// pixels is left at its native resolution; only a genuinely small page gets scaled up.
    pub upscale_min_side_px: u32,
    pub prompt: String,
}

impl OcrConfig {
    /// Reads `OCR_BASE_URL` (required), `OCR_MODEL`/`OCR_API_KEY` (optional). Never logs or
    /// prints `api_key`.
    pub fn from_env() -> Result<Self> {
        let base_url = std::env::var(OCR_BASE_URL_ENV)
            .with_context(|| format!("{OCR_BASE_URL_ENV} env var not set"))?;
        Ok(Self {
            base_url,
            model: std::env::var(OCR_MODEL_ENV).unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
            api_key: std::env::var(OCR_API_KEY_ENV).ok(),
            max_tokens: 8192,
            temperature: 0.0,
            ngram_size: 35,
            window_size: 128,
            page_scale: 2.0,
            upscale_min_side_px: 600,
            prompt: DEFAULT_PROMPT.to_string(),
        })
    }

    /// Cache-invalidation key: every field that changes the OCR OUTPUT for the same image
    /// (prompt, model, decoding, page-split/scale) -- deliberately excludes `base_url`/
    /// `api_key` (which endpoint served the call doesn't change what a deterministic-decoding
    /// call returns) and `page_scale`/`ngram_size`/`window_size` are included because they DO.
    pub fn config_hash(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.model.as_bytes());
        hasher.update(self.prompt.as_bytes());
        hasher.update(self.max_tokens.to_le_bytes());
        hasher.update(self.temperature.to_le_bytes());
        hasher.update(self.ngram_size.to_le_bytes());
        hasher.update(self.window_size.to_le_bytes());
        hasher.update(self.page_scale.to_le_bytes());
        hasher.update(self.upscale_min_side_px.to_le_bytes());
        hasher.update(NEAR_BLACK_LUMA_MAX.to_le_bytes());
        hasher.update(NEAR_BLACK_RUN_MIN_PX.to_le_bytes());
        hasher.update(SLIVER_MAX_PX.to_le_bytes());
        format!("{:x}", hasher.finalize())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OcrUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub seconds: f64,
    pub cache_hit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrPage {
    pub page: u32,
    pub raw_text: String,
    /// Set when the model hit `finish_reason == "length"`, or the reply is a long block of
    /// prose past the point OCR rows stop looking like rows (runaway guard, spec A1). Only
    /// the rows BEFORE the runaway point are ever used by `ocr_parse`/`labels`, and a
    /// truncated page never supplies a final figure unless that figure appears before the
    /// runaway point AND still clears the witness gate.
    pub truncated: bool,
    pub usage: OcrUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrResult {
    pub image_id: String,
    pub pages: Vec<OcrPage>,
}

#[derive(Serialize, Deserialize)]
struct MetaJson {
    model: String,
    config_hash: String,
    pages: Vec<PageMeta>,
}

#[derive(Serialize, Deserialize)]
struct PageMeta {
    page: u32,
    truncated: bool,
    usage: OcrUsage,
}

pub struct OcrClient {
    config: OcrConfig,
    cache_dir: PathBuf,
    http: reqwest::blocking::Client,
}

impl OcrClient {
    pub fn new(config: OcrConfig, cache_dir: PathBuf) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(300))
            // Lead finding 2026-09-13: RunPod's Cloudflare front returns 403 "error code:
            // 1010" for a request with no User-Agent at all (reqwest sends none by default).
            .user_agent("buyorwait-ocr/0.1")
            .build()
            .context("failed to build OCR HTTP client")?;
        Ok(Self { config, cache_dir, http })
    }

    fn image_cache_dir(&self, image_id: &str) -> PathBuf {
        self.cache_dir.join(image_id)
    }

    fn meta_path(&self, image_id: &str) -> PathBuf {
        self.image_cache_dir(image_id).join("meta.json")
    }

    fn page_path(&self, image_id: &str, page: u32) -> PathBuf {
        self.image_cache_dir(image_id).join(format!("page_{page:04}.md"))
    }

    /// Cache-first: split `image_path` into pages, upscale, OCR each page (or reuse the
    /// cache when the on-disk `meta.json`'s `config_hash` matches and `cold` is false),
    /// returning every page's raw text plus usage.
    pub fn ingest(&self, image_id: &str, image_path: &Path, cold: bool) -> Result<OcrResult> {
        let hash = self.config.config_hash();
        if !cold {
            if let Some(cached) = self.read_cache(image_id, &hash)? {
                return Ok(cached);
            }
        }

        let img = image::open(image_path).with_context(|| format!("opening {}", image_path.display()))?;
        let pages = split_pages(&img);
        fs::create_dir_all(self.image_cache_dir(image_id))?;

        let mut result_pages = Vec::with_capacity(pages.len());
        let mut page_metas = Vec::with_capacity(pages.len());
        for (idx, page_img) in pages.iter().enumerate() {
            let page_num = (idx + 1) as u32;
            let (w, h) = page_img.dimensions();
            let shorter_side = w.min(h);
            let final_page = if shorter_side < self.config.upscale_min_side_px {
                upscale(page_img, self.config.page_scale)
            } else {
                page_img.clone()
            };
            let b64 = encode_png_base64(&final_page)?;

            let started = Instant::now();
            let (raw_text, prompt_tokens, completion_tokens, finish_reason) = self.call_once(&b64)?;
            let seconds = started.elapsed().as_secs_f64();
            let truncated = is_runaway(&raw_text, finish_reason.as_deref());

            let usage = OcrUsage { prompt_tokens, completion_tokens, seconds, cache_hit: false };
            fs::write(self.page_path(image_id, page_num), &raw_text)
                .with_context(|| format!("writing OCR cache page {page_num} for {image_id}"))?;
            page_metas.push(PageMeta { page: page_num, truncated, usage: usage.clone() });
            result_pages.push(OcrPage { page: page_num, raw_text, truncated, usage });
        }

        let meta = MetaJson { model: self.config.model.clone(), config_hash: hash, pages: page_metas };
        fs::write(self.meta_path(image_id), serde_json::to_vec_pretty(&meta)?)
            .with_context(|| format!("writing OCR meta.json for {image_id}"))?;

        Ok(OcrResult { image_id: image_id.to_string(), pages: result_pages })
    }

    fn read_cache(&self, image_id: &str, hash: &str) -> Result<Option<OcrResult>> {
        let meta_path = self.meta_path(image_id);
        let Ok(bytes) = fs::read(&meta_path) else { return Ok(None) };
        let Ok(meta) = serde_json::from_slice::<MetaJson>(&bytes) else { return Ok(None) };
        if meta.config_hash != hash {
            return Ok(None);
        }
        let mut pages = Vec::with_capacity(meta.pages.len());
        for pm in meta.pages {
            let Ok(raw_text) = fs::read_to_string(self.page_path(image_id, pm.page)) else { return Ok(None) };
            let mut usage = pm.usage;
            usage.cache_hit = true;
            pages.push(OcrPage { page: pm.page, raw_text, truncated: pm.truncated, usage });
        }
        Ok(Some(OcrResult { image_id: image_id.to_string(), pages }))
    }

    /// One `POST {base_url}/chat/completions` call for one page image. Returns
    /// `(raw_text, prompt_tokens, completion_tokens, finish_reason)`.
    fn call_once(&self, image_b64: &str) -> Result<(String, u64, u64, Option<String>)> {
        let url = format!("{}/chat/completions", self.config.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.config.model,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": self.config.prompt},
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{image_b64}")}},
            ]}],
            "max_tokens": self.config.max_tokens,
            "temperature": self.config.temperature,
            "skip_special_tokens": false,
            "vllm_xargs": {"ngram_size": self.config.ngram_size, "window_size": self.config.window_size},
        });

        let mut req = self.http.post(&url).json(&body);
        if let Some(key) = &self.config.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().context("OCR request failed")?;
        let status = resp.status();
        let text = resp.text().context("reading OCR response body")?;
        if !status.is_success() {
            bail!("OCR request returned {status}: {text}");
        }
        let value: serde_json::Value = serde_json::from_str(&text).context("OCR response is not valid JSON")?;
        let choice = value["choices"].get(0).context("OCR response has no choices")?;
        let raw_text = choice["message"]["content"].as_str().unwrap_or_default().to_string();
        let finish_reason = choice["finish_reason"].as_str().map(str::to_string);
        let prompt_tokens = value["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
        let completion_tokens = value["usage"]["completion_tokens"].as_u64().unwrap_or(0);
        Ok((raw_text, prompt_tokens, completion_tokens, finish_reason))
    }
}

/// `finish_reason == "length"` is the reliable signal; a long block of unstructured prose
/// (no `<|det|>`/`<tr>` markup at all past a certain length) is the fallback heuristic for a
/// truncated reply that still returned `finish_reason == "stop"` (spec A1).
fn is_runaway(raw_text: &str, finish_reason: Option<&str>) -> bool {
    if finish_reason == Some("length") {
        return true;
    }
    const RUNAWAY_CHAR_THRESHOLD: usize = 20_000;
    raw_text.len() > RUNAWAY_CHAR_THRESHOLD && !raw_text.contains("<|det|>") && !raw_text.contains("<tr>")
}

/// Splits `img` at full-width near-black bands: rows whose 99th-percentile luma is below
/// `NEAR_BLACK_LUMA_MAX`, in a run of at least `NEAR_BLACK_RUN_MIN_PX` rows, are treated as a
/// page separator (never included in either resulting page); a resulting page shorter than
/// `SLIVER_MAX_PX` is dropped as a scan artifact, not a real page.
fn split_pages(img: &DynamicImage) -> Vec<DynamicImage> {
    let (width, height) = img.dimensions();
    let gray = img.to_luma8();

    let mut row_p99: Vec<u8> = Vec::with_capacity(height as usize);
    for y in 0..height {
        let mut lumas: Vec<u8> = (0..width).map(|x| gray.get_pixel(x, y)[0]).collect();
        lumas.sort_unstable();
        let idx = ((lumas.len() as f64) * 0.99).floor() as usize;
        row_p99.push(lumas[idx.min(lumas.len().saturating_sub(1))]);
    }

    let mut separators: Vec<(u32, u32)> = Vec::new();
    let mut run_start: Option<u32> = None;
    for y in 0..height {
        if row_p99[y as usize] < NEAR_BLACK_LUMA_MAX {
            run_start.get_or_insert(y);
        } else if let Some(start) = run_start.take() {
            if y - start >= NEAR_BLACK_RUN_MIN_PX {
                separators.push((start, y));
            }
        }
    }
    if let Some(start) = run_start {
        if height - start >= NEAR_BLACK_RUN_MIN_PX {
            separators.push((start, height));
        }
    }

    let mut pages = Vec::new();
    let mut cursor = 0u32;
    for (sep_start, sep_end) in &separators {
        if *sep_start > cursor {
            pages.push((cursor, *sep_start));
        }
        cursor = *sep_end;
    }
    if height > cursor {
        pages.push((cursor, height));
    }

    pages
        .into_iter()
        .filter(|(start, end)| end - start > SLIVER_MAX_PX)
        .map(|(start, end)| img.crop_imm(0, start, width, end - start))
        .collect::<Vec<_>>()
        .into_iter()
        .fold(Vec::new(), |mut acc, p| {
            acc.push(p);
            acc
        })
}

fn upscale(img: &DynamicImage, scale: f64) -> DynamicImage {
    let (w, h) = img.dimensions();
    let new_w = ((w as f64) * scale).round().max(1.0) as u32;
    let new_h = ((h as f64) * scale).round().max(1.0) as u32;
    img.resize(new_w, new_h, image::imageops::FilterType::Lanczos3)
}

fn encode_png_base64(img: &DynamicImage) -> Result<String> {
    let mut buf = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .context("encoding page PNG")?;
    Ok(BASE64.encode(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_hash_changes_when_prompt_or_scale_changes() {
        let base = OcrConfig {
            base_url: "http://x".into(),
            model: DEFAULT_MODEL.into(),
            api_key: None,
            max_tokens: 8192,
            temperature: 0.0,
            ngram_size: 35,
            window_size: 128,
            page_scale: 2.0,
            upscale_min_side_px: 600,
            prompt: DEFAULT_PROMPT.into(),
        };
        let mut scaled = base.clone();
        scaled.page_scale = 3.0;
        assert_ne!(base.config_hash(), scaled.config_hash());

        let mut reprompted = base.clone();
        reprompted.prompt = "different prompt".into();
        assert_ne!(base.config_hash(), reprompted.config_hash());
    }

    #[test]
    fn config_hash_is_stable_for_identical_config() {
        let cfg = OcrConfig {
            base_url: "http://x".into(),
            model: DEFAULT_MODEL.into(),
            api_key: None,
            max_tokens: 8192,
            temperature: 0.0,
            ngram_size: 35,
            window_size: 128,
            page_scale: 2.0,
            upscale_min_side_px: 600,
            prompt: DEFAULT_PROMPT.into(),
        };
        assert_eq!(cfg.config_hash(), cfg.config_hash());
    }

    #[test]
    fn is_runaway_flags_length_finish_reason() {
        assert!(is_runaway("short", Some("length")));
        assert!(!is_runaway("short", Some("stop")));
    }

    #[test]
    fn is_runaway_flags_long_unstructured_prose() {
        let prose = "the quick brown fox ".repeat(2000);
        assert!(is_runaway(&prose, Some("stop")));
    }

    #[test]
    fn is_runaway_never_flags_a_long_well_formed_table() {
        let mut long_table = "<|det|>table [0,0,10,10]<|/det|><table>".to_string();
        for _ in 0..2000 {
            long_table.push_str("<tr><td>Item</td><td>1.00</td></tr>");
        }
        long_table.push_str("</table>");
        assert!(!is_runaway(&long_table, Some("stop")));
    }

    #[test]
    fn split_pages_drops_slivers_and_splits_on_near_black_bands() {
        let mut img = image::RgbImage::new(100, 300);
        for y in 0..300u32 {
            let is_band = (140..150).contains(&y);
            for x in 0..100u32 {
                img.put_pixel(x, y, image::Rgb(if is_band { [0, 0, 0] } else { [255, 255, 255] }));
            }
        }
        let dyn_img = DynamicImage::ImageRgb8(img);
        let pages = split_pages(&dyn_img);
        assert_eq!(pages.len(), 2);
        for p in &pages {
            assert!(p.dimensions().1 > SLIVER_MAX_PX);
        }
    }
}
