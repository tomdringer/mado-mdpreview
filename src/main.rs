//! mado-mdpreview — Markdown preview pixel plugin for Mado
//!
//! stdin:  newline-delimited JSON events from Mado
//!         {"type":"resize","width":600,"height":800}
//!         {"type":"file","path":"/path/to/file.md"}
//!         {"type":"reload"}
//!         {"type":"scroll","delta":-3}
//!
//! stdout: MADO pixel frames  (magic[4] + w[u32le] + h[u32le] + RGBA[w*h*4])

use std::io::{self, BufRead, Write};
use std::fs;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use fontdue::{Font, FontSettings};
use pulldown_cmark::{Parser, Options, Event, Tag, TagEnd, HeadingLevel};
use serde_json::Value;

// ── Font ─────────────────────────────────────────────────────────────────────

static FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/HackNerdFontMono-Regular.ttf");

// ── Colours (light document theme) ───────────────────────────────────────────

const BG:        [u8; 4] = [0xf8, 0xf9, 0xfa, 0xff];
const FG:        [u8; 4] = [0x1a, 0x1a, 0x2e, 0xff];
const H1_BAR:    [u8; 4] = [0x60, 0xa5, 0xfa, 0xff];
const CODE_BG:   [u8; 4] = [0xe8, 0xec, 0xf0, 0xff];
const CODE_FG:   [u8; 4] = [0xc0, 0x39, 0x2b, 0xff];
const RULE_COL:  [u8; 4] = [0xcc, 0xcc, 0xcc, 0xff];
const LINK_FG:   [u8; 4] = [0x27, 0x6b, 0xf5, 0xff];
const QUOTE_BAR: [u8; 4] = [0x94, 0xa3, 0xb8, 0xff];
const QUOTE_FG:  [u8; 4] = [0x64, 0x74, 0x8b, 0xff];

// ── Rendered blocks ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum Block {
    Text        { spans: Vec<Span>, indent: usize },
    Heading     { spans: Vec<Span>, level: u8 },
    Code        (String),
    ListItem    { spans: Vec<Span>, depth: usize },
    OrderedItem { spans: Vec<Span>, n: usize },
    Quote       (Vec<Span>),
    Rule,
    Space       (usize),
}

#[derive(Debug, Clone)]
struct Span {
    text:   String,
    bold:   bool,
    italic: bool,
    code:   bool,
    link:   bool,
}

impl Span {
    fn plain(text: &str) -> Self {
        Span { text: text.to_string(), bold: false, italic: false, code: false, link: false }
    }
}

// ── Markdown → Block list ─────────────────────────────────────────────────────

fn parse_markdown(src: &str) -> Vec<Block> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);

    let mut blocks: Vec<Block> = Vec::new();
    let mut spans:  Vec<Span>  = Vec::new();
    let mut bold         = false;
    let mut italic       = false;
    let mut link         = false;
    let mut in_code      = false;
    let mut code_buf     = String::new();
    let mut h_level: u8  = 0;
    let mut list_depth   = 0usize;
    let mut ordered_n    = 1usize;
    let mut in_ordered   = false;
    let mut in_blockquote = false;
    let mut in_item      = false;

    for event in Parser::new_ext(src, opts) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                h_level = match level {
                    HeadingLevel::H1 => 1, HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3, _ => 4,
                };
                spans.clear();
            }
            Event::End(TagEnd::Heading(_)) => {
                blocks.push(Block::Heading { spans: spans.clone(), level: h_level });
                blocks.push(Block::Space(if h_level == 1 { 8 } else { 4 }));
                spans.clear(); h_level = 0;
            }
            Event::Start(Tag::Paragraph) => { spans.clear(); }
            Event::End(TagEnd::Paragraph) => {
                if in_blockquote {
                    blocks.push(Block::Quote(spans.clone()));
                } else if !in_item {
                    blocks.push(Block::Text { spans: spans.clone(), indent: 0 });
                }
                blocks.push(Block::Space(6));
                spans.clear();
            }
            Event::Start(Tag::Strong)    => { bold = true; }
            Event::End(TagEnd::Strong)   => { bold = false; }
            Event::Start(Tag::Emphasis)  => { italic = true; }
            Event::End(TagEnd::Emphasis) => { italic = false; }
            Event::Start(Tag::Link { .. }) => { link = true; }
            Event::End(TagEnd::Link)       => { link = false; }
            Event::Start(Tag::CodeBlock(_)) => { in_code = true; code_buf.clear(); }
            Event::End(TagEnd::CodeBlock)   => {
                blocks.push(Block::Code(code_buf.trim_end().to_string()));
                blocks.push(Block::Space(6));
                in_code = false;
            }
            Event::Start(Tag::List(start)) => {
                list_depth += 1;
                in_ordered = start.is_some();
                ordered_n  = start.unwrap_or(1) as usize;
            }
            Event::End(TagEnd::List(_)) => {
                list_depth = list_depth.saturating_sub(1);
                if list_depth == 0 { blocks.push(Block::Space(6)); }
            }
            Event::Start(Tag::Item) => { spans.clear(); in_item = true; }
            Event::End(TagEnd::Item) => {
                if in_ordered {
                    blocks.push(Block::OrderedItem { spans: spans.clone(), n: ordered_n });
                    ordered_n += 1;
                } else {
                    blocks.push(Block::ListItem { spans: spans.clone(), depth: list_depth });
                }
                blocks.push(Block::Space(2));
                spans.clear(); in_item = false;
            }
            Event::Start(Tag::BlockQuote(_)) => { in_blockquote = true; }
            Event::End(TagEnd::BlockQuote(_)) => { in_blockquote = false; blocks.push(Block::Space(6)); }
            Event::Rule => {
                blocks.push(Block::Rule);
                blocks.push(Block::Space(6));
            }
            Event::Code(c)  => { spans.push(Span { text: c.to_string(), bold, italic, code: true,  link }); }
            Event::Text(t)  => {
                if in_code { code_buf.push_str(&t); }
                else       { spans.push(Span { text: t.to_string(), bold, italic, code: false, link }); }
            }
            Event::SoftBreak => { spans.push(Span::plain(" ")); }
            Event::HardBreak => {
                if !spans.is_empty() {
                    blocks.push(Block::Text { spans: spans.clone(), indent: 0 });
                    spans.clear();
                }
            }
            _ => {}
        }
    }
    blocks
}

// ── Renderer ──────────────────────────────────────────────────────────────────

struct Renderer {
    font:     Font,
    width:    usize,
    height:   usize,
    buf:      Vec<u8>,
    scroll_y: i32,
}

impl Renderer {
    fn new(font: Font, width: usize, height: usize) -> Self {
        let buf = vec![0u8; width * height * 4];
        Renderer { font, width, height, buf, scroll_y: 0 }
    }

    fn resize(&mut self, w: usize, h: usize) {
        self.width = w; self.height = h;
        self.buf = vec![0u8; w * h * 4];
    }

    fn clear(&mut self) {
        for i in 0..self.width * self.height {
            self.buf[i * 4..i * 4 + 4].copy_from_slice(&BG);
        }
    }

    fn set_pixel(&mut self, x: i32, y: i32, colour: [u8; 4]) {
        if x < 0 || y < 0 { return; }
        let (x, y) = (x as usize, y as usize);
        if x >= self.width || y >= self.height { return; }
        let idx = (y * self.width + x) * 4;
        let a = colour[3] as u32;
        if a == 255 {
            self.buf[idx..idx + 4].copy_from_slice(&colour);
        } else {
            let ia = 255 - a;
            for c in 0..3 {
                self.buf[idx + c] = ((colour[c] as u32 * a + self.buf[idx + c] as u32 * ia) / 255) as u8;
            }
            self.buf[idx + 3] = 255;
        }
    }

    fn fill_rect(&mut self, x: i32, y: i32, w: usize, h: usize, col: [u8; 4]) {
        for dy in 0..h as i32 {
            for dx in 0..w as i32 {
                self.set_pixel(x + dx, y + dy, col);
            }
        }
    }

    fn draw_char(&mut self, ch: char, x: i32, y: i32, size: f32, fg: [u8; 4], bg: Option<[u8; 4]>) -> i32 {
        let idx = self.font.lookup_glyph_index(ch);
        if idx == 0 && ch != ' ' { return (size * 0.6) as i32; }
        let (metrics, bitmap) = self.font.rasterize(ch, size);
        if let Some(bg) = bg {
            self.fill_rect(x, y, (metrics.advance_width as usize).max(metrics.width + 2), size as usize + 4, bg);
        }
        for row in 0..metrics.height {
            for col in 0..metrics.width {
                let alpha = bitmap[row * metrics.width + col];
                if alpha == 0 { continue; }
                let px = x + col as i32 + metrics.xmin;
                let py = y + (size as i32 - metrics.height as i32 - metrics.ymin) + row as i32;
                let mut c = fg; c[3] = alpha;
                self.set_pixel(px, py, c);
            }
        }
        metrics.advance_width as i32
    }

    fn draw_str(&mut self, s: &str, x: i32, y: i32, size: f32, fg: [u8; 4], bg: Option<[u8; 4]>) -> i32 {
        let mut cx = x;
        for ch in s.chars() { cx += self.draw_char(ch, cx, y, size, fg, bg); }
        cx
    }

    fn measure(&self, s: &str, size: f32) -> i32 {
        s.chars().map(|ch| self.font.metrics(ch, size).advance_width as i32).sum()
    }

    fn wrap_spans<'a>(&self, spans: &'a [Span], size: f32, max_w: i32) -> Vec<Vec<(&'a Span, String)>> {
        let mut lines: Vec<Vec<(&Span, String)>> = vec![vec![]];
        let mut line_w = 0i32;
        for span in spans {
            for word in span.text.split_inclusive(' ') {
                let w = self.measure(word, size);
                if line_w + w > max_w && line_w > 0 { lines.push(vec![]); line_w = 0; }
                if let Some(line) = lines.last_mut() { line.push((span, word.to_string())); line_w += w; }
            }
        }
        lines
    }

    fn render(&mut self, blocks: &[Block]) {
        self.clear();
        let pad   = 24i32;
        let max_w = self.width as i32 - pad * 2;
        let bsz   = 15.0f32;
        let lh    = 22i32;
        let mut cy = pad - self.scroll_y;

        for block in blocks {
            match block {
                Block::Space(px) => { cy += *px as i32; }

                Block::Rule => {
                    self.fill_rect(pad, cy + 4, max_w as usize, 1, RULE_COL);
                    cy += 12;
                }

                Block::Heading { spans, level } => {
                    let (sz, lh2) = match level {
                        1 => (28.0f32, 36i32), 2 => (22.0, 30), 3 => (18.0, 26), _ => (16.0, 24),
                    };
                    for line in self.wrap_spans(spans, sz, max_w) {
                        if cy + lh2 > 0 && cy < self.height as i32 {
                            let mut cx = pad;
                            for (span, word) in &line {
                                let fg = if span.link { LINK_FG } else { FG };
                                cx += self.draw_str(word, cx, cy, sz, fg, None);
                            }
                            if *level == 1 {
                                self.fill_rect(pad, cy + lh2 - 2, max_w as usize, 2, H1_BAR);
                            }
                        }
                        cy += lh2;
                    }
                }

                Block::Text { spans, indent } => {
                    let iw = *indent as i32 * 16;
                    for line in self.wrap_spans(spans, bsz, max_w - iw) {
                        if cy + lh > 0 && cy < self.height as i32 {
                            let mut cx = pad + iw;
                            for (span, word) in &line {
                                let (fg, bg) = if span.code { (CODE_FG, Some(CODE_BG)) }
                                               else if span.link { (LINK_FG, None) }
                                               else { (FG, None) };
                                let sz = if span.bold { bsz + 1.0 } else { bsz };
                                cx += self.draw_str(word, cx, cy, sz, fg, bg);
                            }
                        }
                        cy += lh;
                    }
                }

                Block::Code(src) => {
                    let csz = 13.0f32;
                    let clh = 19i32;
                    let ip  = 8i32;
                    let bh  = src.lines().count() as i32 * clh + ip * 2;
                    if cy + bh > 0 && cy < self.height as i32 {
                        self.fill_rect(pad, cy, max_w as usize, bh as usize, CODE_BG);
                    }
                    let mut iy = cy + ip;
                    for line in src.lines() {
                        if iy + clh > 0 && iy < self.height as i32 {
                            self.draw_str(line, pad + ip, iy, csz, FG, None);
                        }
                        iy += clh;
                    }
                    cy += bh;
                }

                Block::ListItem { spans, depth } => {
                    let ix = pad + *depth as i32 * 16;
                    if cy + lh > 0 && cy < self.height as i32 {
                        self.draw_char('•', ix - 12, cy, bsz, FG, None);
                    }
                    for line in self.wrap_spans(spans, bsz, max_w - *depth as i32 * 16) {
                        if cy + lh > 0 && cy < self.height as i32 {
                            let mut cx = ix;
                            for (span, word) in &line {
                                let sz = if span.bold { bsz + 1.0 } else { bsz };
                                let fg = if span.link { LINK_FG } else { FG };
                                cx += self.draw_str(word, cx, cy, sz, fg, None);
                            }
                        }
                        cy += lh;
                    }
                }

                Block::OrderedItem { spans, n } => {
                    let label = format!("{}. ", n);
                    let lw    = self.measure(&label, bsz);
                    if cy + lh > 0 && cy < self.height as i32 {
                        self.draw_str(&label, pad, cy, bsz, FG, None);
                    }
                    for line in self.wrap_spans(spans, bsz, max_w - lw) {
                        if cy + lh > 0 && cy < self.height as i32 {
                            let mut cx = pad + lw;
                            for (span, word) in &line {
                                let sz = if span.bold { bsz + 1.0 } else { bsz };
                                cx += self.draw_str(word, cx, cy, sz, FG, None);
                            }
                        }
                        cy += lh;
                    }
                }

                Block::Quote(spans) => {
                    self.fill_rect(pad, cy, 3, lh as usize, QUOTE_BAR);
                    for line in self.wrap_spans(spans, bsz, max_w - 16) {
                        if cy + lh > 0 && cy < self.height as i32 {
                            let mut cx = pad + 14;
                            for (span, word) in &line { cx += self.draw_str(word, cx, cy, bsz, QUOTE_FG, None); }
                        }
                        cy += lh;
                    }
                }
            }
        }
    }

    fn write_frame(&self) {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        let _ = out.write_all(b"MADO");
        let _ = out.write_all(&(self.width as u32).to_le_bytes());
        let _ = out.write_all(&(self.height as u32).to_le_bytes());
        let _ = out.write_all(&self.buf);
        let _ = out.flush();
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() {
    let font   = Font::from_bytes(FONT_BYTES, FontSettings::default()).expect("font load failed");
    let state  = Arc::new(Mutex::new(Renderer::new(font, 600, 800)));
    let path:  Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let blocks: Arc<Mutex<Vec<Block>>>    = Arc::new(Mutex::new(Vec::new()));
    let dirty:  Arc<Mutex<bool>>          = Arc::new(Mutex::new(false));

    // File watcher: re-reads file every 500ms, sets dirty on change
    {
        let path_r = Arc::clone(&path);
        let blk_r  = Arc::clone(&blocks);
        let dir_r  = Arc::clone(&dirty);
        thread::spawn(move || {
            let mut last_content = String::new();
            loop {
                thread::sleep(Duration::from_millis(500));
                if let Some(p) = path_r.lock().unwrap().clone() {
                    if let Ok(src) = fs::read_to_string(&p) {
                        if src != last_content {
                            *blk_r.lock().unwrap() = parse_markdown(&src);
                            *dir_r.lock().unwrap() = true;
                            last_content = src;
                        }
                    }
                }
            }
        });
    }

    // Render thread: flushes a frame when dirty
    {
        let state_r = Arc::clone(&state);
        let blk_r   = Arc::clone(&blocks);
        let dir_r   = Arc::clone(&dirty);
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_millis(100));
                let is_dirty = { let mut d = dir_r.lock().unwrap(); if *d { *d = false; true } else { false } };
                if is_dirty {
                    let blks = blk_r.lock().unwrap().clone();
                    let mut r = state_r.lock().unwrap();
                    r.render(&blks);
                    r.write_frame();
                }
            }
        });
    }

    // Event loop: read JSON events from Mado
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(val)  = serde_json::from_str::<Value>(&line) else { continue };
        match val["type"].as_str().unwrap_or("") {
            "resize" => {
                let w = val["width"].as_u64().unwrap_or(600) as usize;
                let h = val["height"].as_u64().unwrap_or(800) as usize;
                state.lock().unwrap().resize(w, h);
                *dirty.lock().unwrap() = true;
            }
            "file" => {
                if let Some(p) = val["path"].as_str() {
                    *path.lock().unwrap() = Some(p.to_string());
                    state.lock().unwrap().scroll_y = 0;
                    *dirty.lock().unwrap() = true;
                }
            }
            "reload" => { *dirty.lock().unwrap() = true; }
            "scroll" => {
                let delta = val["delta"].as_i64().unwrap_or(0) as i32;
                let mut r = state.lock().unwrap();
                r.scroll_y = (r.scroll_y - delta * 40).max(0);
                *dirty.lock().unwrap() = true;
            }
            _ => {}
        }
    }
}
