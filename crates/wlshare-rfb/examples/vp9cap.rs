//! What is made of a capture ([`wlshare_rfb::capture`]): the file a daemon
//! started with `--capture-frames` wrote of what its VP9 encoder was handed,
//! played into the encoder again, as it is pinned now, and written as the
//! streams a decoder is tested and timed on.
//!
//! ```text
//! vp9cap info CAPTURE
//! vp9cap shape STREAM.ivf
//! vp9cap streams CAPTURE --dir DIR --name NAME [--least N] [--threads N]
//! vp9cap sample CAPTURE --dir DIR --name NAME --frames N [--busiest]
//!        [--threads N]
//! ```
//!
//! A capture ending in `.zst` is read through `zstd -dc`, which must then be
//! on the path.
//!
//! `info` says what a capture holds: for each run of frames at one size, how
//! many there are, how many were whole pictures, how much of the picture the
//! rest said changed, and how long the run lasted.
//!
//! `shape` says what `sample` names a stream for, of any IVF: its size, its
//! tile columns and whether it is loop filtered, as its first frame has them.
//!
//! `streams` codes every frame as the session did — its rectangles, its dial,
//! its keyframe where one was asked — into one stream per run of a size,
//! `DIR/NAME-WxH-444.ivf`, 4:4:4 being the one chroma there is, with `-2`,
//! `-3` for a size the capture comes back to. A run of fewer than `--least` frames is left
//! out: the second or two a session spent at another size before it was
//! resized into its own is in a capture for an encoder's sake, and is no
//! stream to test a decoder on. Beside each is `.ivf.csv`, `frame,ms,bytes,keyframe,quality`: the
//! time is when the session's encoder was handed the frame, counted from the
//! stream's first, and it is the IVF timestamp too. This is the file pair
//! the remotex gateway's `--vp9-capture` writes, from a source that can be
//! coded again.
//!
//! `sample` cuts one stream of exactly N frames from the capture's longest
//! run of a size: its first N, or with `--busiest` the N in a row whose
//! frames said the most pixels changed, the earliest such where several tie.
//! The cut starts with a keyframe of the picture as it stood, and codes the
//! frames after it as the session did. It is named for what a decoder's
//! threads make of it, read from the keyframe's own header and not from the
//! encoder's rule: `DIR/NAME-WxH-<tile columns>col-<lf|nolf>.ivf`. A run
//! shorter than N is an error.
//!
//! Each command prints the path of every stream it wrote, one a line.
//!
//! `--threads` is the encoder's, 4 by default: the tile columns a width is
//! coded in are capped by them, so a sample is the same stream on any
//! machine only at the same count.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use screen_vp9_native::Stream;
use wlshare_rfb::capture::{Frame, Reader};

type Error = Box<dyn std::error::Error>;

struct Args {
    capture: PathBuf,
    dir: PathBuf,
    name: String,
    frames: usize,
    least: u32,
    busiest: bool,
    threads: usize,
}

const USAGE: &str = "usage: vp9cap info CAPTURE
       vp9cap shape STREAM.ivf
       vp9cap streams CAPTURE --dir DIR --name NAME [--least N] [--threads N]
       vp9cap sample CAPTURE --dir DIR --name NAME --frames N [--busiest] [--threads N]";

fn parse_args() -> Result<(String, Args), String> {
    let mut args = std::env::args().skip(1);
    let command = args.next().ok_or("a command")?;
    let capture = PathBuf::from(args.next().ok_or("a capture to read")?);
    let mut parsed = Args { capture, dir: PathBuf::new(), name: String::new(), frames: 0, least: 1, busiest: false, threads: 4 };
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{flag} takes a value"));
        match flag.as_str() {
            "--dir" => parsed.dir = PathBuf::from(value()?),
            "--name" => parsed.name = value()?,
            "--frames" => parsed.frames = value()?.parse().map_err(|_| "--frames takes a count")?,
            "--least" => parsed.least = value()?.parse().map_err(|_| "--least takes a count")?,
            "--busiest" => parsed.busiest = true,
            "--threads" => parsed.threads = value()?.parse().map_err(|_| "--threads takes a count")?,
            other => return Err(format!("{other}: not a flag")),
        }
    }
    match command.as_str() {
        "info" | "shape" => {}
        "streams" | "sample" => {
            if parsed.dir.as_os_str().is_empty() || parsed.name.is_empty() {
                return Err("--dir and --name are needed".into());
            }
            if parsed.threads == 0 {
                return Err("--threads takes at least 1".into());
            }
            if (command == "sample") != (parsed.frames > 0) || (parsed.busiest && command != "sample") {
                return Err("--frames and --busiest are sample's, and --frames is needed there".into());
            }
        }
        other => return Err(format!("{other}: not a command")),
    }
    Ok((command, parsed))
}

fn main() {
    let (command, args) = match parse_args() {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!("vp9cap: {e}\n{USAGE}");
            std::process::exit(2);
        }
    };
    let done = match command.as_str() {
        "info" => info(&args),
        "shape" => shape(&args),
        "streams" => streams(&args),
        _ => sample(&args),
    };
    if let Err(e) = done {
        eprintln!("vp9cap: {}: {e}", args.capture.display());
        std::process::exit(1);
    }
}

// ── The capture in ───────────────────────────────────────────────────────────

/// A capture open for reading, with the `zstd` it is read through, if any.
struct Source {
    reader: Reader<Box<dyn Read>>,
    zstd: Option<Child>,
}

impl Source {
    fn open(path: &Path) -> Result<Self, Error> {
        let (input, zstd): (Box<dyn Read>, _) = if path.extension().is_some_and(|e| e == "zst") {
            // Opened here so that a file that is not there is this program's to say.
            let file = File::open(path)?;
            let mut child = Command::new("zstd").arg("-dc").stdin(file).stdout(Stdio::piped()).spawn().map_err(|e| format!("running zstd: {e}"))?;
            (Box::new(child.stdout.take().expect("piped")), Some(child))
        } else {
            (Box::new(File::open(path)?), None)
        };
        Ok(Self { reader: Reader::new(Box::new(BufReader::with_capacity(1 << 20, input)) as Box<dyn Read>)?, zstd })
    }

    /// After the capture's end was read: whether `zstd` read all of its own.
    fn finish(mut self) -> Result<(), Error> {
        if let Some(mut child) = self.zstd.take()
            && !child.wait()?.success()
        {
            return Err("zstd could not decompress it".into());
        }
        Ok(())
    }
}

impl Drop for Source {
    /// A capture left before its end: `zstd` has nobody to write to.
    fn drop(&mut self) {
        if let Some(child) = &mut self.zstd {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ── info ─────────────────────────────────────────────────────────────────────

/// What one frame of a capture is, without its pixels.
struct Seen {
    size: (u16, u16),
    micros: u64,
    quality: u8,
    keyframe: bool,
    whole: bool,
    changed: u64,
}

fn scan(path: &Path) -> Result<Vec<Seen>, Error> {
    let mut source = Source::open(path)?;
    let mut seen = Vec::new();
    while let Some(frame) = source.reader.next()? {
        seen.push(Seen {
            size: frame.size,
            micros: frame.at.as_micros() as u64,
            quality: frame.quality,
            keyframe: frame.keyframe,
            whole: frame.changed.is_none(),
            changed: frame.changed_pixels(),
        });
    }
    source.finish()?;
    Ok(seen)
}

/// The runs of frames at one size, as ranges of frame indices.
fn runs(seen: &[Seen]) -> Vec<std::ops::Range<usize>> {
    let mut runs: Vec<std::ops::Range<usize>> = Vec::new();
    for (i, frame) in seen.iter().enumerate() {
        match runs.last_mut() {
            Some(run) if seen[run.start].size == frame.size => run.end = i + 1,
            _ => runs.push(i..i + 1),
        }
    }
    runs
}

fn info(args: &Args) -> Result<(), Error> {
    let seen = scan(&args.capture)?;
    println!("{} frames", seen.len());
    for run in runs(&seen) {
        let frames = &seen[run.clone()];
        let (width, height) = frames[0].size;
        let whole = frames.iter().filter(|f| f.whole).count();
        let keyframes = frames.iter().filter(|f| f.keyframe).count();
        let partial: Vec<&Seen> = frames.iter().filter(|f| !f.whole).collect();
        let share = if partial.is_empty() { 0.0 } else { partial.iter().map(|f| f.changed).sum::<u64>() as f64 * 100.0 / (partial.len() as f64 * f64::from(width) * f64::from(height)) };
        let seconds = (frames[frames.len() - 1].micros - frames[0].micros) as f64 / 1e6;
        let (low, high) = frames.iter().fold((u8::MAX, 0), |(low, high), f| (low.min(f.quality), high.max(f.quality)));
        println!(
            "{width}x{height}: frames {}..{}, {:.1} s, {whole} whole, {keyframes} keyframes asked, the other {} changed {share:.1}% of the picture each, quality {low}..{high}",
            run.start,
            run.end - 1,
            seconds,
            partial.len(),
        );
    }
    Ok(())
}

// ── The streams out ──────────────────────────────────────────────────────────

/// Where an IVF header keeps its frame count.
const FRAME_COUNT_AT: u64 = 24;

/// An IVF header for a VP9 stream of a `width`×`height` picture timed in
/// milliseconds, its frame count to be written when the frames are.
fn ivf_header(width: u16, height: u16) -> [u8; 32] {
    let mut header = [0; 32];
    header[..4].copy_from_slice(b"DKIF");
    header[6..8].copy_from_slice(&32u16.to_le_bytes());
    header[8..12].copy_from_slice(b"VP90");
    header[12..14].copy_from_slice(&width.to_le_bytes());
    header[14..16].copy_from_slice(&height.to_le_bytes());
    header[16..20].copy_from_slice(&1000u32.to_le_bytes());
    header[20..24].copy_from_slice(&1u32.to_le_bytes());
    header
}

/// One stream being coded from a capture's frames and written: the encoder,
/// the IVF and its CSV. Written under a name of its own making and given the
/// name it is to keep when it is whole, so a stream cut short is never taken
/// for one.
struct Output {
    stream: Stream,
    size: (u16, u16),
    ivf: BufWriter<File>,
    csv: BufWriter<File>,
    part: PathBuf,
    began: Option<u64>,
    frames: u32,
    /// The stream's first frame, which says how the rest are coded.
    first: Vec<u8>,
    unit: Vec<u8>,
}

impl Output {
    fn new(dir: &Path, size: (u16, u16), quality: u8, threads: usize) -> Result<Self, Error> {
        let part = dir.join(format!(".vp9cap-{}.part", std::process::id()));
        let mut ivf = BufWriter::new(File::create(&part)?);
        ivf.write_all(&ivf_header(size.0, size.1))?;
        let mut csv = BufWriter::new(File::create(part.with_extension("csv"))?);
        csv.write_all(b"frame,ms,bytes,keyframe,quality\n")?;
        Ok(Self { stream: Stream::new(size.0, size.1, quality, threads)?, size, ivf, csv, part, began: None, frames: 0, first: Vec::new(), unit: Vec::new() })
    }

    /// Code `frame` as the session did, or as a keyframe of the whole picture
    /// where the stream starts with it.
    fn frame(&mut self, frame: &Frame<'_>) -> Result<(), Error> {
        let starting = self.began.is_none();
        if self.stream.quality() != frame.quality {
            self.stream.set_quality(frame.quality);
        }
        self.unit.clear();
        let changed = if starting { None } else { frame.changed };
        self.stream.read_bgrx(frame.pixels, usize::from(self.size.0) * 4, changed)?;
        let keyframe = self.stream.encode(starting || frame.keyframe, &mut self.unit)?;
        let micros = frame.at.as_micros() as u64;
        let ms = (micros - *self.began.get_or_insert(micros)) / 1000;
        if starting {
            self.first.clone_from(&self.unit);
        }
        self.ivf.write_all(&(self.unit.len() as u32).to_le_bytes())?;
        self.ivf.write_all(&ms.to_le_bytes())?;
        self.ivf.write_all(&self.unit)?;
        writeln!(self.csv, "{},{ms},{},{},{}", self.frames, self.unit.len(), u8::from(keyframe), frame.quality)?;
        self.frames += 1;
        Ok(())
    }

    /// Write the frame count, and give the files the name `path` and its
    /// `.csv`, printed.
    fn finish(mut self, path: &Path) -> Result<(), Error> {
        self.ivf.seek(SeekFrom::Start(FRAME_COUNT_AT))?;
        self.ivf.write_all(&self.frames.to_le_bytes())?;
        self.ivf.flush()?;
        self.csv.flush()?;
        std::fs::rename(&self.part, path)?;
        let mut csv = path.as_os_str().to_owned();
        csv.push(".csv");
        std::fs::rename(self.part.with_extension("csv"), csv)?;
        println!("{}", path.display());
        Ok(())
    }

    /// Remove what a stream that is not to be kept has written.
    fn discard(self) {
        let _ = std::fs::remove_file(&self.part);
        let _ = std::fs::remove_file(self.part.with_extension("csv"));
    }
}

fn streams(args: &Args) -> Result<(), Error> {
    std::fs::create_dir_all(&args.dir)?;
    let mut source = Source::open(&args.capture)?;
    let mut output: Option<Output> = None;
    let mut written: Vec<(u16, u16)> = Vec::new();
    let path = |written: &mut Vec<(u16, u16)>, size: (u16, u16)| {
        written.push(size);
        let nth = written.iter().filter(|&&s| s == size).count();
        let again = if nth > 1 { format!("-{nth}") } else { String::new() };
        args.dir.join(format!("{}-{}x{}-444{again}.ivf", args.name, size.0, size.1))
    };
    let keep = |done: Output, written: &mut Vec<(u16, u16)>| {
        if done.frames < args.least.max(1) {
            done.discard();
            return Ok(());
        }
        let size = done.size;
        done.finish(&path(written, size))
    };
    while let Some(frame) = source.reader.next()? {
        if let Some(done) = output.take_if(|o| o.size != frame.size) {
            keep(done, &mut written)?;
        }
        let output = match &mut output {
            Some(output) => output,
            None => output.insert(Output::new(&args.dir, frame.size, frame.quality, args.threads)?),
        };
        output.frame(&frame)?;
    }
    source.finish()?;
    keep(output.ok_or("the capture holds no frames")?, &mut written)
}

fn sample(args: &Args) -> Result<(), Error> {
    let seen = scan(&args.capture)?;
    let run = runs(&seen).into_iter().max_by_key(|run| (run.len(), std::cmp::Reverse(run.start))).ok_or("the capture holds no frames")?;
    let (width, height) = seen[run.start].size;
    if run.len() < args.frames {
        return Err(format!("its longest run of a size is {} frames of {width}x{height}, short of the {} asked", run.len(), args.frames).into());
    }
    let mut start = run.start;
    if args.busiest {
        let mut window: u64 = seen[start..start + args.frames].iter().map(|f| f.changed).sum();
        let mut most = window;
        for at in run.start + 1..=run.end - args.frames {
            window = window - seen[at - 1].changed + seen[at + args.frames - 1].changed;
            if window > most {
                (most, start) = (window, at);
            }
        }
    }

    std::fs::create_dir_all(&args.dir)?;
    let mut source = Source::open(&args.capture)?;
    let mut output: Option<Output> = None;
    while let Some(frame) = source.reader.next()? {
        if (frame.index as usize) < start {
            continue;
        }
        if frame.index as usize >= start + args.frames {
            break;
        }
        let output = match &mut output {
            Some(output) => output,
            None => output.insert(Output::new(&args.dir, frame.size, frame.quality, args.threads)?),
        };
        output.frame(&frame)?;
    }
    drop(source);
    let output = output.expect("the run is in the capture");
    let Some(shape) = keyframe_shape(&output.first) else {
        output.discard();
        return Err("the stream's first frame is not a keyframe this reads".into());
    };
    eprintln!("vp9cap: {}: frames {start}..{} of {}", args.name, start + args.frames - 1, seen.len());
    let path = args.dir.join(format!("{}-{}.ivf", args.name, shape.name()));
    output.finish(&path)
}

fn shape(args: &Args) -> Result<(), Error> {
    let mut ivf = Vec::new();
    File::open(&args.capture)?.take(1 << 16).read_to_end(&mut ivf)?;
    if ivf.len() < 44 || &ivf[..4] != b"DKIF" || &ivf[8..12] != b"VP90" {
        return Err("not a VP9 stream in IVF".into());
    }
    let shape = keyframe_shape(&ivf[44..]).ok_or("the stream's first frame is not a keyframe this reads")?;
    println!("{}", shape.name());
    Ok(())
}

// ── What a keyframe says of its stream ───────────────────────────────────────

/// What a decoder's threads have to go on, as a keyframe's header has it.
struct Shape {
    width: u32,
    height: u32,
    /// The loop filter's level, 0 for none.
    loop_filter_level: u8,
    tile_columns_log2: u32,
}

impl Shape {
    /// `WxH-<tile columns>col-<lf|nolf>`.
    fn name(&self) -> String {
        let filter = if self.loop_filter_level > 0 { "lf" } else { "nolf" };
        format!("{}x{}-{}col-{filter}", self.width, self.height, 1u32 << self.tile_columns_log2)
    }
}

/// A frame's bits, most significant first; `None` past its end.
struct Bits<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn take(&mut self, count: u32) -> Option<u32> {
        let mut value = 0;
        for _ in 0..count {
            let byte = *self.bytes.get(self.at / 8)?;
            value = value << 1 | u32::from(byte >> (7 - self.at % 8) & 1);
            self.at += 1;
        }
        Some(value)
    }

    fn flag(&mut self) -> Option<bool> {
        Some(self.take(1)? == 1)
    }

    /// A field that is there only behind a set bit.
    fn optional(&mut self, count: u32) -> Option<()> {
        if self.flag()? {
            self.take(count)?;
        }
        Some(())
    }
}

/// Read a keyframe's uncompressed header (VP9 bitstream §6.2) as far as its
/// tile columns, for the loop filter's level and the columns: the two things
/// a stream's name says. `None` for a frame that is not a keyframe of an
/// 8-bit stream, or whose header runs out.
fn keyframe_shape(frame: &[u8]) -> Option<Shape> {
    let mut bits = Bits { bytes: frame, at: 0 };
    if bits.take(2)? != 2 {
        return None;
    }
    let low = bits.take(1)?;
    let profile = bits.take(1)? << 1 | low;
    // Profiles 2 and 3 are high bit depth, which nothing here codes.
    // show_existing_frame, then frame_type, where 0 is a keyframe.
    if profile > 1 || bits.flag()? || bits.flag()? {
        return None;
    }
    let _show_frame = bits.flag()?;
    let error_resilient = bits.flag()?;
    if bits.take(24)? != 0x49_83_42 {
        return None;
    }
    // color_config: the colour space, and unless it is sRGB the range, and
    // in profile 1 the subsampling and a reserved bit.
    let srgb = bits.take(3)? == 7;
    if !srgb {
        bits.take(1)?;
    }
    if profile == 1 {
        bits.take(if srgb { 1 } else { 3 })?;
    }
    let width = bits.take(16)? + 1;
    let height = bits.take(16)? + 1;
    // render_size, when it differs.
    bits.optional(32)?;
    if !error_resilient {
        // refresh_frame_context and frame_parallel_decoding_mode.
        bits.take(2)?;
    }
    let _frame_context = bits.take(2)?;

    // loop_filter_params.
    let loop_filter_level = bits.take(6)? as u8;
    let _sharpness = bits.take(3)?;
    if bits.flag()? && bits.flag()? {
        // The four reference deltas and the two mode deltas, each behind a bit.
        for _ in 0..6 {
            bits.optional(7)?;
        }
    }
    // quantization_params: the base index and three deltas behind a bit each.
    bits.take(8)?;
    for _ in 0..3 {
        bits.optional(5)?;
    }
    // segmentation_params.
    if bits.flag()? {
        if bits.flag()? {
            for _ in 0..7 {
                bits.optional(8)?;
            }
            if bits.flag()? {
                for _ in 0..3 {
                    bits.optional(8)?;
                }
            }
        }
        if bits.flag()? {
            bits.take(1)?;
            for _ in 0..8 {
                // Each feature's value and its sign: the quantizer, the loop
                // filter, the reference, and skip, which has no value.
                for value in [8, 6, 2, 0] {
                    if bits.flag()? && value > 0 {
                        bits.take(value + u32::from(value != 2))?;
                    }
                }
            }
        }
    }
    // tile_info: the columns' log2, counted up from the least the width allows
    // a bit at a time to the most.
    let superblocks = width.div_ceil(64);
    let mut least = 0;
    while (64 << least) < superblocks {
        least += 1;
    }
    let mut most = 1;
    while (superblocks >> most) >= 4 {
        most += 1;
    }
    most -= 1;
    let mut tile_columns_log2 = least;
    while tile_columns_log2 < most && bits.flag()? {
        tile_columns_log2 += 1;
    }
    Some(Shape { width, height, loop_filter_level, tile_columns_log2 })
}
