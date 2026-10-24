//! Screenshot pixel tolerance (`pixels.py`), stray-shot ownership (`shots.py`),
//! batch refresh commit helpers (`refresh.py`), and redraw diagnostics (`trainbuild.py`)
//! (`R-SH-1`..`R-SH-3`).

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

pub const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
pub const DEFAULT_PIXEL_EDGE: u8 = 24;
pub const DEFAULT_PIXEL_SHARE_PPM: u32 = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedPng {
    pub width: u32,
    pub height: u32,
    pub channels: usize,
    pub rows: Vec<Vec<u8>>,
}

/// Decodes an 8-bit non-interlaced RGB (`color_type = 2`) or RGBA (`color_type = 6`) PNG.
/// Returns `None` on any malformed, interlaced, or unsupported PNG (fail closed: counts as moved).
pub fn decode_png(data: Option<&[u8]>) -> Option<DecodedPng> {
    let data = data?;
    if !data.starts_with(PNG_SIGNATURE) {
        return None;
    }
    let (mut pos, mut header, mut idat) = (PNG_SIGNATURE.len(), None, Vec::new());
    while pos.checked_add(8)? <= data.len() {
        let size = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
        let kind = &data[pos + 4..pos + 8];
        let body_end = pos.checked_add(8)?.checked_add(size)?;
        let chunk_end = body_end.checked_add(4)?;
        if chunk_end > data.len() {
            return None;
        }
        let body = &data[pos + 8..body_end];
        pos = chunk_end;
        if kind == b"IHDR" {
            if body.len() < 13 {
                return None;
            }
            let w = u32::from_be_bytes(body[0..4].try_into().ok()?);
            let h = u32::from_be_bytes(body[4..8].try_into().ok()?);
            header = Some((w, h, body[8], body[9], body[12]));
        } else if kind == b"IDAT" {
            idat.extend_from_slice(body);
        }
    }
    let (width, height, bit_depth, color_type, interlace) = header?;
    if width == 0 || height == 0 || bit_depth != 8 || interlace != 0 {
        return None;
    }
    let step: usize = match color_type {
        2 => 3,
        6 => 4,
        _ => return None,
    };
    let raw = zlib_decompress(&idat)?;
    let stride = (width as usize).checked_mul(step)?;
    if raw.len() != (height as usize).checked_mul(stride.checked_add(1)?)? {
        return None;
    }
    let (mut rows, mut prev) = (Vec::with_capacity(height as usize), vec![0u8; stride]);
    for y in 0..(height as usize) {
        let start = y * (stride + 1);
        let mut row = raw[start + 1..start + 1 + stride].to_vec();
        unfilter(raw[start], &mut row, &prev, step)?;
        prev.clone_from(&row);
        rows.push(row);
    }
    Some(DecodedPng {
        width,
        height,
        channels: step,
        rows,
    })
}

fn unfilter(kind: u8, row: &mut [u8], up: &[u8], step: usize) -> Option<()> {
    for i in 0..row.len() {
        let left = if i >= step {
            i32::from(row[i - step])
        } else {
            0
        };
        let above = i32::from(up[i]);
        let corner = if i >= step {
            i32::from(up[i - step])
        } else {
            0
        };
        let delta = match kind {
            0 => 0,
            1 => left,
            2 => above,
            3 => (left + above) / 2,
            4 => {
                let g = left + above - corner;
                let (dl, da, dc) = ((g - left).abs(), (g - above).abs(), (g - corner).abs());
                if dl <= da && dl <= dc {
                    left
                } else if da <= dc {
                    above
                } else {
                    corner
                }
            }
            _ => return None,
        };
        row[i] = ((i32::from(row[i]) + delta) & 0xff) as u8;
    }
    Some(())
}

/// Decides whether `after` is a visibly different screenshot from `before` (`R-SH-1`).
pub fn pixel_moved(
    before: Option<&[u8]>,
    after: Option<&[u8]>,
    golden: bool,
    edge: u8,
    share_ppm: u32,
) -> bool {
    if before.is_some() && before == after {
        return false;
    }
    let (Some(one), Some(two)) = (decode_png(before), decode_png(after)) else {
        return true;
    };
    if (one.width, one.height, one.channels) != (two.width, two.height, two.channels) {
        return true;
    }
    let mut differing: u64 = 0;
    for (ra, rb) in one.rows.iter().zip(two.rows.iter()) {
        if ra == rb {
            continue;
        }
        if golden {
            return true;
        }
        for (&x, &y) in ra.iter().zip(rb.iter()) {
            if x.abs_diff(y) > edge {
                differing += 1;
            }
        }
    }
    let total = u64::from(one.width) * u64::from(one.height) * (one.channels as u64);
    (differing as f64) > (f64::from(share_ppm) / 1_000_000.0) * (total as f64)
}

pub fn moved(before: Option<&[u8]>, after: Option<&[u8]>, golden: bool) -> bool {
    pixel_moved(
        before,
        after,
        golden,
        DEFAULT_PIXEL_EDGE,
        DEFAULT_PIXEL_SHARE_PPM,
    )
}

/// Encodes a valid 8-bit non-interlaced RGBA PNG using stored zlib blocks.
pub fn encode_rgba_png(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((height as usize) * ((width as usize) * 4 + 1));
    for y in 0..height {
        raw.push(0);
        for x in 0..width {
            raw.extend_from_slice(&pixel(x, y));
        }
    }
    let mut z = vec![0x78, 0x01];
    let chunks: Vec<&[u8]> = if raw.is_empty() {
        vec![&[]]
    } else {
        raw.chunks(65_535).collect()
    };
    for (i, chunk) in chunks.iter().enumerate() {
        z.push(u8::from(i + 1 == chunks.len()));
        let len = chunk.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(chunk);
    }
    let (mut s1, mut s2) = (1u32, 0u32);
    for &b in &raw {
        s1 = (s1 + u32::from(b)) % 65_521;
        s2 = (s2 + s1) % 65_521;
    }
    z.extend_from_slice(&((s2 << 16) | s1).to_be_bytes());
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    let mut out = PNG_SIGNATURE.to_vec();
    for (kind, data) in [
        (b"IHDR", ihdr.as_slice()),
        (b"IDAT", z.as_slice()),
        (b"IEND", &[][..]),
    ] {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&0u32.to_be_bytes());
    }
    out
}

fn zlib_decompress(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 6 {
        return None;
    }
    let (cmf, flg) = (data[0], data[1]);
    if (cmf & 0x0f) != 8 || ((u16::from(cmf) << 8 | u16::from(flg)) % 31) != 0 {
        return None;
    }
    let start = if (flg & 0x20) != 0 { 6 } else { 2 };
    if start + 4 > data.len() {
        return None;
    }
    let mut bits = BitReader {
        data: &data[start..data.len() - 4],
        byte_pos: 0,
        bit_pos: 0,
    };
    let mut out = Vec::new();
    loop {
        let bfinal = bits.read_bits(1)?;
        match bits.read_bits(2)? {
            0 => {
                bits.align_byte();
                let (len, nlen) = (bits.read_u16_le()? as usize, bits.read_u16_le()? as usize);
                if (len ^ 0xffff) != nlen {
                    return None;
                }
                out.extend_from_slice(bits.read_bytes(len)?);
            }
            bt @ (1 | 2) => {
                let (lt, dt) = if bt == 1 {
                    (HuffTree::fixed_lit()?, HuffTree::fixed_dist()?)
                } else {
                    read_dynamic_trees(&mut bits)?
                };
                decode_huffman_block(&mut bits, &lt, &dt, &mut out)?;
            }
            _ => return None,
        }
        if bfinal == 1 {
            break;
        }
    }
    Some(out)
}

struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8,
}

impl<'a> BitReader<'a> {
    fn read_bit(&mut self) -> Option<u32> {
        let b = *self.data.get(self.byte_pos)?;
        let bit = u32::from((b >> self.bit_pos) & 1);
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
        Some(bit)
    }
    fn read_bits(&mut self, n: u8) -> Option<u32> {
        let mut val = 0u32;
        for i in 0..n {
            val |= self.read_bit()? << i;
        }
        Some(val)
    }
    fn align_byte(&mut self) {
        if self.bit_pos != 0 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
    }
    fn read_u16_le(&mut self) -> Option<u16> {
        let s = self.read_bytes(2)?;
        Some(u16::from_le_bytes([s[0], s[1]]))
    }
    fn read_bytes(&mut self, len: usize) -> Option<&'a [u8]> {
        self.align_byte();
        let end = self.byte_pos.checked_add(len)?;
        let slice = self.data.get(self.byte_pos..end)?;
        self.byte_pos = end;
        Some(slice)
    }
}

struct HuffTree {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl HuffTree {
    fn from_lengths(lengths: &[u8]) -> Option<Self> {
        let mut counts = [0u16; 16];
        for &l in lengths {
            if l as usize >= counts.len() {
                return None;
            }
            if l > 0 {
                counts[l as usize] += 1;
            }
        }
        let (mut offsets, mut total) = ([0u16; 16], 0u16);
        for bits in 1..16 {
            offsets[bits] = total;
            total += counts[bits];
        }
        let mut symbols = vec![0u16; total as usize];
        for (sym, &l) in lengths.iter().enumerate() {
            if l > 0 {
                let idx = offsets[l as usize] as usize;
                offsets[l as usize] += 1;
                symbols[idx] = sym as u16;
            }
        }
        Some(Self { counts, symbols })
    }
    fn fixed_lit() -> Option<Self> {
        let mut lengths = vec![0u8; 288];
        lengths[0..144].fill(8);
        lengths[144..256].fill(9);
        lengths[256..280].fill(7);
        lengths[280..288].fill(8);
        Self::from_lengths(&lengths)
    }
    fn fixed_dist() -> Option<Self> {
        Self::from_lengths(&[5u8; 32])
    }
    fn decode(&self, bits: &mut BitReader<'_>) -> Option<u16> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= bits.read_bit()? as i32;
            let count = i32::from(self.counts[len]);
            if code - count < first {
                return self.symbols.get((index + (code - first)) as usize).copied();
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        None
    }
}

fn read_dynamic_trees(bits: &mut BitReader<'_>) -> Option<(HuffTree, HuffTree)> {
    let hlit = bits.read_bits(5)? as usize + 257;
    let hdist = bits.read_bits(5)? as usize + 1;
    let hclen = bits.read_bits(4)? as usize + 4;
    const ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let mut cl_lens = [0u8; 19];
    for i in 0..hclen {
        cl_lens[ORDER[i]] = bits.read_bits(3)? as u8;
    }
    let cl_tree = HuffTree::from_lengths(&cl_lens)?;
    let mut lens = Vec::with_capacity(hlit + hdist);
    while lens.len() < hlit + hdist {
        match cl_tree.decode(bits)? {
            sym @ 0..=15 => lens.push(sym as u8),
            16 => {
                let prev = *lens.last()?;
                lens.extend(std::iter::repeat_n(prev, bits.read_bits(2)? as usize + 3));
            }
            17 => lens.extend(std::iter::repeat_n(0, bits.read_bits(3)? as usize + 3)),
            18 => lens.extend(std::iter::repeat_n(0, bits.read_bits(7)? as usize + 11)),
            _ => return None,
        }
    }
    (lens.len() == hlit + hdist).then_some((
        HuffTree::from_lengths(&lens[..hlit])?,
        HuffTree::from_lengths(&lens[hlit..])?,
    ))
}

fn decode_huffman_block(
    bits: &mut BitReader<'_>,
    lit_tree: &HuffTree,
    dist_tree: &HuffTree,
    out: &mut Vec<u8>,
) -> Option<()> {
    const LEN_BASE: [usize; 29] = [
        3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
        131, 163, 195, 227, 258,
    ];
    const LEN_EXTRA: [u8; 29] = [
        0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
    ];
    const DIST_BASE: [usize; 30] = [
        1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
        2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
    ];
    const DIST_EXTRA: [u8; 30] = [
        0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
        13, 13,
    ];
    loop {
        let sym = lit_tree.decode(bits)?;
        if sym < 256 {
            out.push(sym as u8);
        } else if sym == 256 {
            return Some(());
        } else {
            let l_idx = (sym as usize).checked_sub(257)?;
            let length = *LEN_BASE.get(l_idx)? + bits.read_bits(*LEN_EXTRA.get(l_idx)?)? as usize;
            let d_sym = dist_tree.decode(bits)? as usize;
            let dist = *DIST_BASE.get(d_sym)? + bits.read_bits(*DIST_EXTRA.get(d_sym)?)? as usize;
            if dist == 0 || dist > out.len() {
                return None;
            }
            for _ in 0..length {
                let b = out[out.len() - dist];
                out.push(b);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShotRules {
    pub area_roots: Vec<(String, String)>,
    pub shot_dirs: Vec<(String, String)>,
    pub generated_prefixes: Vec<String>,
    pub aliases: BTreeMap<String, String>,
    pub shared_compose: BTreeSet<String>,
    pub screenshot_tests_marker: String,
    pub screen_dirs: BTreeMap<String, String>,
}

impl Default for ShotRules {
    fn default() -> Self {
        Self {
            area_roots: vec![
                ("console".into(), "frontend/admin/".into()),
                ("android".into(), "frontend/android/".into()),
            ],
            shot_dirs: vec![
                ("console".into(), "frontend/admin/screenshots/".into()),
                (
                    "android".into(),
                    "frontend/android/docs/screenshots/".into(),
                ),
            ],
            generated_prefixes: vec![
                "frontend/admin/screenshots/".into(),
                "frontend/admin/docs/".into(),
                "frontend/android/docs/".into(),
            ],
            aliases: BTreeMap::from([
                ("home".into(), "dashboard".into()),
                ("settings".into(), "preferences".into()),
                ("funding".into(), "treasury".into()),
                ("provider".into(), "treasury".into()),
            ]),
            shared_compose: BTreeSet::from([
                "components".into(),
                "theme".into(),
                "design".into(),
                "nav".into(),
                "system".into(),
            ]),
            screenshot_tests_marker: "/screenshot/".into(),
            screen_dirs: BTreeMap::from([("console".into(), "frontend/admin/src/screens".into())]),
        }
    }
}

fn moves_no_pixel_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(\.md$|\.test\.[jt]sx?$|\.spec\.[jt]s$|/e2e/|/docs/|/androidTest/|\.golden$)")
            .expect("static regex")
    })
}

fn first_word(name: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^[A-Z][a-z0-9]*|^[a-z0-9]+").expect("static regex"));
    re.find(name)
        .map_or_else(String::new, |m| m.as_str().to_ascii_lowercase())
}

fn normal_alnum(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

pub fn stray_shots<S: AsRef<str>>(
    files: &[S],
    golden: &BTreeSet<String>,
    rules: &ShotRules,
) -> Vec<String> {
    let is_shot = |p: &str| {
        p.ends_with(".png")
            && rules
                .shot_dirs
                .iter()
                .any(|(_, d)| p.starts_with(d.as_str()))
    };
    let area_of = |p: &str| -> Option<&str> {
        rules
            .area_roots
            .iter()
            .find(|(_, r)| p.starts_with(r.as_str()))
            .map(|(a, _)| a.as_str())
    };
    let (mut owned, mut every) = (BTreeMap::<String, BTreeSet<String>>::new(), BTreeSet::new());

    for raw in files {
        let f = raw.as_ref();
        if is_shot(f) {
            continue;
        }
        let Some(area) = area_of(f) else {
            continue;
        };
        if rules
            .generated_prefixes
            .iter()
            .any(|g| f.starts_with(g.as_str()))
            || moves_no_pixel_re().is_match(f)
        {
            continue;
        }
        let (dir, base) = f.rsplit_once('/').unwrap_or(("", f));
        let stem = base.rsplit_once('.').map_or(base, |(s, _)| s);
        if let Some(screen_dir) = rules.screen_dirs.get(area) {
            if dir == screen_dir {
                let mut w = first_word(stem);
                if w.ends_with('s') && w.len() > 1 {
                    w.pop();
                }
                let alias = rules.aliases.get(&w).cloned().unwrap_or_else(|| w.clone());
                let entry = owned.entry(area.to_string()).or_default();
                entry.insert(w);
                entry.insert(alias);
            } else {
                every.insert(area.to_string());
            }
            continue;
        }
        if !rules.screenshot_tests_marker.is_empty() && f.contains(&rules.screenshot_tests_marker) {
            every.insert(area.to_string());
            continue;
        }
        if f.contains("/src/test/") {
            continue;
        }
        if let Some((_, after_ui)) = f.split_once("/ui/") {
            if let Some((pkg, _)) = after_ui.split_once('/') {
                if !rules.shared_compose.contains(pkg) {
                    let entry = owned.entry(area.to_string()).or_default();
                    entry.insert(pkg.to_string());
                    entry.insert(first_word(stem));
                    continue;
                }
            }
        }
        every.insert(area.to_string());
    }

    let mut out = Vec::new();
    for raw in files {
        let f = raw.as_ref();
        if !is_shot(f) || golden.contains(f) {
            continue;
        }
        let Some(area) = area_of(f) else {
            continue;
        };
        if every.contains(area) {
            continue;
        }
        let base = f.rsplit_once('/').map_or(f, |(_, b)| b);
        let name = normal_alnum(base);
        let matched = owned.get(area).is_some_and(|ws| {
            ws.iter()
                .map(|w| normal_alnum(w))
                .filter(|w| !w.is_empty())
                .any(|w| name.contains(&w))
        });
        if !matched {
            out.push(f.to_string());
        }
    }
    out
}

pub fn stray_refusal(paths: &[String], tool: &str, base_env: &str) -> String {
    let listed = paths.join(" ");
    format!(
        "{} screenshot(s) changed that no screen in this diff draws: {listed}. The pipeline \
         redraws shots on the merged tree; commit only the golden ones and those of the screens \
         you changed. Drop them: git checkout ${base_env} -- {listed} && git commit -m \
         'Leave the screenshots to the batch', or run `{tool} pr --shots-anyway` if the diff \
         really moves them",
        paths.len()
    )
}

pub fn shot_stem(path: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(-dark|-phone|-fr)+$").expect("static regex"));
    let base = path.rsplit_once('/').map_or(path, |(_, b)| b);
    let stem = base.rsplit_once('.').map_or(base, |(s, _)| s);
    re.replace(stem, "").into_owned()
}

pub fn moved_summary<S: AsRef<str>>(moved: &[S]) -> String {
    if moved.is_empty() {
        return "no shot moved".into();
    }
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for path in moved {
        *counts.entry(shot_stem(path.as_ref())).or_insert(0) += 1;
    }
    let screens: Vec<String> = counts
        .into_iter()
        .map(|(s, n)| if n > 1 { format!("{s} ({n})") } else { s })
        .collect();
    format!(
        "{} shot{} moved: {}",
        moved.len(),
        if moved.len() == 1 { "" } else { "s" },
        screens.join(", ")
    )
}

pub fn golden_names<S: AsRef<str>>(
    golden_files: &[S],
    mut read: impl FnMut(&str) -> String,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for gf in golden_files {
        let gf = gf.as_ref();
        let dir = gf.strip_suffix(".golden").unwrap_or(gf);
        for line in read(gf).lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                out.insert(format!("{dir}/{trimmed}"));
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StaleShotsStep {
    None,
    Wait,
    Redraw,
}

impl StaleShotsStep {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Wait => "wait",
            Self::Redraw => "redraw",
        }
    }
}

pub fn stale_shots_step(
    unowned: bool,
    main: &str,
    recorded_main: Option<&str>,
    opened_at: i64,
    now: i64,
    cooldown_secs: i64,
) -> StaleShotsStep {
    if !unowned || recorded_main == Some(main) {
        StaleShotsStep::None
    } else if now - opened_at < cooldown_secs {
        StaleShotsStep::Wait
    } else {
        StaleShotsStep::Redraw
    }
}

pub fn format_redraw_failure(command_line: &str, output: &str, log_path: &str) -> (String, String) {
    let saved = format!("$ {command_line}\n{output}");
    let lines: Vec<&str> = saved.lines().collect();
    let tail = lines[lines.len().saturating_sub(30)..].join("\n");
    let detail = if tail.is_empty() {
        format!("full output: {log_path}")
    } else {
        format!("full output: {log_path}\n{tail}")
    };
    (saved, detail)
}

pub fn android_redraw_command(gradle_slot: &str) -> String {
    let prefix = if gradle_slot.trim().is_empty() {
        String::new()
    } else {
        format!("{} ", gradle_slot.trim())
    };
    format!("{prefix}./gradlew testDebugUnitTest --rerun -Proborazzi.test.record=true")
}

pub fn refresh_commit_subject(label: &str) -> String {
    format!("Screenshots for train {label}")
}
