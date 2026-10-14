//! Synthetic screen-share source whose frames carry their capture time.
//!
//! A row of large black/white blocks at the top encodes the wall-clock time in milliseconds, so a
//! viewer on the same machine (or with synchronized clocks) can measure capture → encode → SFU →
//! decode latency without a camera. Blocks are big enough to survive lossy encoding. The rest of
//! the frame scrolls a gradient so the encoder has real work to do.

/// Bits of the encoded timestamp: milliseconds modulo 2^40 (about 35 years) is plenty.
const BITS: usize = 40;
/// Block size in pixels. Encoders blur edges; sampling block centers tolerates that.
const BLOCK: u32 = 16;
/// Start marker (white, black, white, black) so a frame without a code is not misread.
const MARKER: [bool; 4] = [true, false, true, false];

const WHITE: u8 = 235;
const BLACK: u8 = 16;

/// Smallest frame that fits the code.
pub const MIN_WIDTH: u32 = (MARKER.len() + BITS) as u32 * BLOCK;

/// Fills an I420 luma plane: the timestamp code in the first block row, a moving gradient below.
pub fn draw(
    y: &mut [u8],
    stride: u32,
    width: u32,
    height: u32,
    timestamp_ms: u64,
    frame_index: u64,
) {
    let stride = stride as usize;
    for row in 0..height as usize {
        let line = &mut y[row * stride..row * stride + width as usize];
        if (row as u32) < BLOCK {
            for (column, pixel) in line.iter_mut().enumerate() {
                let block = column / BLOCK as usize;
                *pixel = match bit(block, timestamp_ms) {
                    Some(true) => WHITE,
                    Some(false) | None => BLACK,
                };
            }
        } else {
            let shift = (frame_index * 4) as usize;
            for (column, pixel) in line.iter_mut().enumerate() {
                *pixel = ((column + row + shift) % 220 + 16) as u8;
            }
        }
    }
}

/// Reads the timestamp back from a decoded luma plane, or `None` when no code is present.
pub fn read(y: &[u8], stride: u32, width: u32, height: u32) -> Option<u64> {
    if width < MIN_WIDTH || height < BLOCK {
        return None;
    }
    let center_row = (BLOCK / 2) as usize * stride as usize;
    let sample = |block: usize| y[center_row + block * BLOCK as usize + BLOCK as usize / 2] > 128;
    if MARKER
        .iter()
        .enumerate()
        .any(|(i, &expected)| sample(i) != expected)
    {
        return None;
    }
    Some((0..BITS).fold(0u64, |value, i| {
        (value << 1) | u64::from(sample(MARKER.len() + i))
    }))
}

fn bit(block: usize, timestamp_ms: u64) -> Option<bool> {
    if block < MARKER.len() {
        return Some(MARKER[block]);
    }
    let index = block - MARKER.len();
    (index < BITS).then(|| (timestamp_ms >> (BITS - 1 - index)) & 1 == 1)
}

/// Milliseconds as encoded (modulo 2^40), for comparing with a decoded value.
pub fn truncate(timestamp_ms: u64) -> u64 {
    timestamp_ms & ((1u64 << BITS) - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_timestamp() {
        let (width, height) = (1280, 720);
        let mut y = vec![0u8; (width * height) as usize];
        let now = 1_760_000_123_456;

        draw(&mut y, width, width, height, now, 7);

        assert_eq!(read(&y, width, width, height), Some(truncate(now)));
    }

    #[test]
    fn survives_moderate_noise() {
        let (width, height) = (1280, 720);
        let mut y = vec![0u8; (width * height) as usize];
        draw(&mut y, width, width, height, 987_654_321, 0);
        for (i, pixel) in y.iter_mut().enumerate() {
            let noise = (i * 7919 % 61) as i16 - 30;
            *pixel = (i16::from(*pixel) + noise).clamp(0, 255) as u8;
        }

        assert_eq!(read(&y, width, width, height), Some(987_654_321));
    }

    #[test]
    fn ignores_frames_without_a_code() {
        let (width, height) = (1280, 720);
        let y = vec![128u8; (width * height) as usize];

        assert_eq!(read(&y, width, width, height), None);
        assert_eq!(read(&y, width, MIN_WIDTH - 1, height), None);
    }
}
