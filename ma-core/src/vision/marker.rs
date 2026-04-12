// /Memory-Archive/ma-core/src/vision/marker.rs

use anyhow::Context;
use image::{DynamicImage, Rgba, RgbaImage};
use imageproc::drawing::{
    draw_filled_circle_mut, draw_filled_rect_mut, draw_hollow_circle_mut,
    draw_hollow_rect_mut, draw_line_segment_mut,
};
use imageproc::rect::Rect;

// Visual constants
const CIRCLE_RADIUS: i32 = 5;
const ARROW_GAP: i32     = 5;   // gap between circle edge and arrow start
const ARROW_LENGTH: i32  = 65;  // length of arrow shaft
const ARROWHEAD: i32     = 9;   // arrowhead arm length
const BOX_PAD: i32       = 8;   // padding inside coordinate box
const FONT_SCALE: u32    = 2;   // render 5×7 font at 2× scale → 10×14px
const CHAR_W: u32        = 5 * FONT_SCALE;
const CHAR_H: u32        = 7 * FONT_SCALE;
const CHAR_SP: u32       = FONT_SCALE; // 1px inter-char spacing, scaled

const RED:   Rgba<u8> = Rgba([220, 38,  38,  255]); // #dc2626 — click markers
const BLUE:  Rgba<u8> = Rgba([59,  130, 246, 255]); // #3b82f6 — type markers
const WHITE: Rgba<u8> = Rgba([255, 255, 255, 255]);
const BLACK: Rgba<u8> = Rgba([0,   0,   0,   255]);

// Direction

#[derive(Clone, Copy)]
enum Dir { Right, Left, Down, Up }

// ── Public API
//
/// Apply click annotation to raw image bytes.
///
/// Draws a filled red circle at (click_x, click_y), a directional arrow, and a
/// coordinate box showing the exact pixel coordinates.
///
/// `image_bytes` — raw image bytes from The-Eyes.
/// `ext`         — file extension derived from the Content-Type header (e.g. "webp", "png", "jpg").
///                 The output is encoded in the same format as the input.
pub fn mark(image_bytes: &[u8], click_x: i32, click_y: i32, ext: &str) -> anyhow::Result<Vec<u8>> {
    let img = image::load_from_memory(image_bytes)
        .context("Failed to decode image from The-Eyes")?;

    let mut canvas = img.to_rgba8();
    let (img_w, img_h) = canvas.dimensions();

    // Clamp click coords to valid pixel range.
    let cx = click_x.clamp(0, img_w as i32 - 1);
    let cy = click_y.clamp(0, img_h as i32 - 1);

    let label    = format!("X: {}  Y: {}", click_x, click_y);
    let text_w   = text_pixel_width(&label);
    let box_w    = text_w as i32 + BOX_PAD * 2;
    let box_h    = CHAR_H as i32 + BOX_PAD * 2;

    let dir = best_direction(cx, cy, img_w, img_h, box_w, box_h);

    let (a_start, a_end, raw_box) = arrow_and_box(cx, cy, box_w, box_h, dir);
    let box_origin = clamp_box(raw_box, box_w, box_h, img_w as i32, img_h as i32);

    // Circle: filled red + thin black outline
    draw_filled_circle_mut(&mut canvas, (cx, cy), CIRCLE_RADIUS, RED);
    draw_hollow_circle_mut(&mut canvas, (cx, cy), CIRCLE_RADIUS, BLACK);

    // Arrow shaft (draw twice, 1px apart, for 2px thickness)
    for off in 0..2i32 {
        let (ox, oy) = match dir {
            Dir::Right | Dir::Left => (0, off),
            Dir::Down  | Dir::Up   => (off, 0),
        };
        draw_line_segment_mut(
            &mut canvas,
            ((a_start.0 + ox) as f32, (a_start.1 + oy) as f32),
            ((a_end.0   + ox) as f32, (a_end.1   + oy) as f32),
            RED,
        );
    }

    // Arrowhead
    draw_arrowhead(&mut canvas, a_end, dir, RED);

    // Coordinate box: white fill → red inner border → black outer border
    let box_rect = Rect::at(box_origin.0, box_origin.1)
        .of_size(box_w as u32, box_h as u32);
    draw_filled_rect_mut(&mut canvas, box_rect, WHITE);
    draw_hollow_rect_mut(
        &mut canvas,
        Rect::at(box_origin.0 + 1, box_origin.1 + 1)
            .of_size(box_w as u32 - 2, box_h as u32 - 2),
        RED,
    );
    draw_hollow_rect_mut(&mut canvas, box_rect, BLACK);

    // Label text
    draw_text(
        &mut canvas,
        &label,
        (box_origin.0 + BOX_PAD) as u32,
        (box_origin.1 + BOX_PAD) as u32,
        BLACK,
    );

    encode_canvas(canvas, ext)
}

/// Apply type-event annotation to image bytes.
///
/// Draws any combination of:
///   - A blue hollow rectangle around the detected text field (`diff_rect`)
///   - A hollow blue circle at the detected cursor insertion point (`cursor_pos`)
///     with a directional arrow and coordinate box
///
/// Both are optional. If both are None, the image is returned unchanged.
/// The caller is responsible for choosing the appropriate `suffix` (`_type` vs `_unmarked`).
///
/// `diff_rect` — (x, y, w, h) bounding box of changed pixels between before and after frames.
/// `cursor_pos` — (x, y) midpoint of detected cursor strip.
///
/// `ext` — file extension from Content-Type header. Output is encoded in the same format.
#[allow(dead_code)]
pub fn mark_type(
    image_bytes: &[u8],
    diff_rect: Option<(i32, i32, u32, u32)>,
    cursor_pos: Option<(i32, i32)>,
    ext: &str,
) -> anyhow::Result<Vec<u8>> {
    let img = image::load_from_memory(image_bytes)
        .context("Failed to decode image for type annotation")?;

    let mut canvas = img.to_rgba8();
    let (img_w, img_h) = canvas.dimensions();

    // Diff rect — hollow blue rectangle around the text field
    if let Some((rx, ry, rw, rh)) = diff_rect {
        let rx = rx.clamp(0, img_w as i32 - 1);
        let ry = ry.clamp(0, img_h as i32 - 1);
        let rw = rw.min(img_w.saturating_sub(rx as u32)).max(1);
        let rh = rh.min(img_h.saturating_sub(ry as u32)).max(1);

        // 2px thick border via two concentric hollow rects.
        let outer = Rect::at(rx, ry).of_size(rw, rh);
        draw_hollow_rect_mut(&mut canvas, outer, BLUE);
        if rw > 2 && rh > 2 {
            let inner = Rect::at(rx + 1, ry + 1).of_size(rw - 2, rh - 2);
            draw_hollow_rect_mut(&mut canvas, inner, BLUE);
        }
    }

    // Cursor annotation — hollow blue circle + arrow + coordinate box
    if let Some((cx, cy)) = cursor_pos {
        let cx = cx.clamp(0, img_w as i32 - 1);
        let cy = cy.clamp(0, img_h as i32 - 1);

        // Hollow circle (2px thick = two concentric hollow circles).
        draw_hollow_circle_mut(&mut canvas, (cx, cy), CIRCLE_RADIUS, BLUE);
        if CIRCLE_RADIUS > 1 {
            draw_hollow_circle_mut(&mut canvas, (cx, cy), CIRCLE_RADIUS - 1, BLUE);
        }

        let label  = format!("X: {}  Y: {}", cx, cy);
        let text_w = text_pixel_width(&label);
        let box_w  = text_w as i32 + BOX_PAD * 2;
        let box_h  = CHAR_H as i32 + BOX_PAD * 2;

        let dir = best_direction(cx, cy, img_w, img_h, box_w, box_h);
        let (a_start, a_end, raw_box) = arrow_and_box(cx, cy, box_w, box_h, dir);
        let box_origin = clamp_box(raw_box, box_w, box_h, img_w as i32, img_h as i32);

        // Arrow shaft
        for off in 0..2i32 {
            let (ox, oy) = match dir {
                Dir::Right | Dir::Left => (0, off),
                Dir::Down  | Dir::Up   => (off, 0),
            };
            draw_line_segment_mut(
                &mut canvas,
                ((a_start.0 + ox) as f32, (a_start.1 + oy) as f32),
                ((a_end.0   + ox) as f32, (a_end.1   + oy) as f32),
                BLUE,
            );
        }
        draw_arrowhead(&mut canvas, a_end, dir, BLUE);

        // Coordinate box: white fill → blue inner border → black outer border
        let box_rect = Rect::at(box_origin.0, box_origin.1)
            .of_size(box_w as u32, box_h as u32);
        draw_filled_rect_mut(&mut canvas, box_rect, WHITE);
        draw_hollow_rect_mut(
            &mut canvas,
            Rect::at(box_origin.0 + 1, box_origin.1 + 1)
                .of_size(box_w as u32 - 2, box_h as u32 - 2),
            BLUE,
        );
        draw_hollow_rect_mut(&mut canvas, box_rect, BLACK);
        draw_text(
            &mut canvas,
            &label,
            (box_origin.0 + BOX_PAD) as u32,
            (box_origin.1 + BOX_PAD) as u32,
            BLACK,
        );
    }

    encode_canvas(canvas, ext)
}

// Direction logic
fn best_direction(cx: i32, cy: i32, img_w: u32, img_h: u32, box_w: i32, box_h: i32) -> Dir {
    let need_h = CIRCLE_RADIUS + ARROW_GAP + ARROW_LENGTH + box_w;
    let need_v = CIRCLE_RADIUS + ARROW_GAP + ARROW_LENGTH + box_h;

    let candidates: [(i32, Dir); 4] = [
        (img_w as i32 - cx - need_h, Dir::Right),
        (cx            - need_h,     Dir::Left),
        (img_h as i32 - cy - need_v, Dir::Down),
        (cy            - need_v,     Dir::Up),
    ];

    candidates
        .into_iter()
        .max_by_key(|(score, _)| *score)
        .map(|(_, dir)| dir)
        .unwrap_or(Dir::Right)
}

/// Returns (arrow_start, arrow_end, box_top_left) — all in image coordinates.
fn arrow_and_box(
    cx: i32, cy: i32,
    box_w: i32, box_h: i32,
    dir: Dir,
) -> ((i32, i32), (i32, i32), (i32, i32)) {
    match dir {
        Dir::Right => {
            let x0 = cx + CIRCLE_RADIUS + ARROW_GAP;
            let x1 = x0 + ARROW_LENGTH;
            ((x0, cy), (x1, cy), (x1, cy - box_h / 2))
        }
        Dir::Left => {
            let x0 = cx - CIRCLE_RADIUS - ARROW_GAP;
            let x1 = x0 - ARROW_LENGTH;
            ((x0, cy), (x1, cy), (x1 - box_w, cy - box_h / 2))
        }
        Dir::Down => {
            let y0 = cy + CIRCLE_RADIUS + ARROW_GAP;
            let y1 = y0 + ARROW_LENGTH;
            ((cx, y0), (cx, y1), (cx - box_w / 2, y1))
        }
        Dir::Up => {
            let y0 = cy - CIRCLE_RADIUS - ARROW_GAP;
            let y1 = y0 - ARROW_LENGTH;
            ((cx, y0), (cx, y1), (cx - box_w / 2, y1 - box_h))
        }
    }
}

/// Clamp box origin so the box stays fully within the image.
fn clamp_box(origin: (i32, i32), box_w: i32, box_h: i32, img_w: i32, img_h: i32) -> (i32, i32) {
    let x = origin.0.clamp(0, (img_w - box_w).max(0));
    let y = origin.1.clamp(0, (img_h - box_h).max(0));
    (x, y)
}

// Arrowhead
fn draw_arrowhead(canvas: &mut RgbaImage, tip: (i32, i32), dir: Dir, color: Rgba<u8>) {
    let (tx, ty) = tip;
    let a = ARROWHEAD;

    // Two lines forming a V at the tip.
    let arms: [((i32, i32), (i32, i32)); 2] = match dir {
        Dir::Right => [((tx, ty), (tx - a, ty - a / 2)), ((tx, ty), (tx - a, ty + a / 2))],
        Dir::Left  => [((tx, ty), (tx + a, ty - a / 2)), ((tx, ty), (tx + a, ty + a / 2))],
        Dir::Down  => [((tx, ty), (tx - a / 2, ty - a)), ((tx, ty), (tx + a / 2, ty - a))],
        Dir::Up    => [((tx, ty), (tx - a / 2, ty + a)), ((tx, ty), (tx + a / 2, ty + a))],
    };

    for (p1, p2) in arms {
        draw_line_segment_mut(
            canvas,
            (p1.0 as f32, p1.1 as f32),
            (p2.0 as f32, p2.1 as f32),
            color,
        );
    }
}

// Bitmap font
//
// 5 columns × 7 rows per character.
// Each row is a u8 with bits 4:0 used — bit 4 = leftmost column.

fn char_bitmap(c: char) -> [u8; 7] {
    match c {
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00110, 0b01000, 0b10000, 0b11111],
        '3' => [0b01110, 0b10001, 0b00001, 0b00110, 0b00001, 0b10001, 0b01110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b00100, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        'X' => [0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001],
        'Y' => [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100],
        ':' => [0b00000, 0b00100, 0b00100, 0b00000, 0b00100, 0b00100, 0b00000],
        _   => [0; 7], // space and any other char → blank
    }
}

fn text_pixel_width(text: &str) -> u32 {
    let n = text.chars().count() as u32;
    if n == 0 {
        return 0;
    }
    n * CHAR_W + (n - 1) * CHAR_SP
}

fn draw_text(canvas: &mut RgbaImage, text: &str, x: u32, y: u32, color: Rgba<u8>) {
    let (img_w, img_h) = canvas.dimensions();
    let mut cursor = x;

    for ch in text.chars() {
        let bitmap = char_bitmap(ch);
        for (row, &bits) in bitmap.iter().enumerate() {
            for col in 0..5u32 {
                if (bits >> (4 - col)) & 1 == 1 {
                    for dy in 0..FONT_SCALE {
                        for dx in 0..FONT_SCALE {
                            let px = cursor + col * FONT_SCALE + dx;
                            let py = y + row as u32 * FONT_SCALE + dy;
                            if px < img_w && py < img_h {
                                canvas.put_pixel(px, py, color);
                            }
                        }
                    }
                }
            }
        }
        cursor += CHAR_W + CHAR_SP;
    }
}

// Encode helpers
fn ext_to_format(ext: &str) -> image::ImageFormat {
    match ext.to_lowercase().as_str() {
        "jpg" | "jpeg" => image::ImageFormat::Jpeg,
        "webp"         => image::ImageFormat::WebP,
        "bmp"          => image::ImageFormat::Bmp,
        "tiff" | "tif" => image::ImageFormat::Tiff,
        _              => image::ImageFormat::Png,
    }
}

fn encode_canvas(canvas: RgbaImage, ext: &str) -> anyhow::Result<Vec<u8>> {
    let format = ext_to_format(ext);
    let img = DynamicImage::ImageRgba8(canvas);
    let img = if format == image::ImageFormat::Jpeg {
        DynamicImage::ImageRgb8(img.to_rgb8())
    } else {
        img
    };
    let mut out: Vec<u8> = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), format)
        .with_context(|| format!("Failed to encode image as {ext}"))?;
    Ok(out)
}