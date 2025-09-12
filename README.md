# slycer

Command-line tool to download YouTube audio and split it into chapter tracks.

## Features
- Chapter-aware splitting
- Batch mode: read a file with URLs
- Cross-platform (macOS, Linux, Windows)

## Requirements
- `yt-dlp`
- `ffmpeg`

If missing, slycer can install them for you (`--yes`).

## Install
```bash
cargo install --path .
# or build locally
cargo build --release
```

## Usage
```bash
slycer <INPUT> [flags]
```
Where `<INPUT>` is either a single YouTube URL or a path to a text file
with one URL per line.

### Flags
- `-o, --output <FILE>`: temporary combined audio file name (default: `out.mp3`)
- `-f, --audio-format <FMT>`: audio format for final tracks (default: `mp3`)
- `-d, --dest <DIR>`: destination directory for split tracks (created if missing)
- `-k, --keep`: keep the combined audio file after splitting
- `-y, --yes`: auto-install missing dependencies
- `--prefix <STR>`: add custom prefix to each output filename
- `--prefix-name`: add video title-derived prefix (first segment before ` - `, `(` or `[`, lowercased, sanitized, max 40 chars)
- `--numbers`: add zero-padded track numbers (width based on chapter count)

### Examples
```bash
# Single URL
yt_url="https://www.youtube.com/watch?v=..."
slycer "$yt_url" --dest tracks --numbers --prefix=wow --audio-format m4a --yes

# Batch file
echo "https://www.youtube.com/watch?v=..." > urls.txt
echo "not-a-link" >> urls.txt
slycer urls.txt --dest out --numbers --prefix-name --yes
```

## License
MIT — see `LICENSE`.
