# 🍙 waku

A fast, lightweight, `ani-cli` style CLI tool for streaming Thai-subbed anime from **anime-waku.com** directly in your terminal and playing it via **mpv**.

Written in Rust for speed and simplicity.

---

## ✨ Features

* **⚡ Smart Fuzzy Search:** Blazing fast search that understands English titles, Thai titles, Romaji, typos, and missing punctuation (e.g., handles `re zero`, `re:zero`, and `rezero` effortlessly).
* **🎬 Direct Stream Sniffing:** Automatically bypasses site embeds and disguised stream players (including `doodee-player` HLS playlists) to feed raw video streams straight to `mpv`.
* **🎛️ Interactive Controls:** Easy-to-use terminal UI to select anime, pick seasons/episodes, change servers, switch video quality, or jump to the next/previous episode after watching.
* **📺 Quality Picker:** Choose between `1080p`, `720p`, `480p`, `360p`, or let it auto-select `best`/`worst`. Remembers your preference for future sessions.
* **📦 Portable Executable Setup:** Automatically detects `mpv` and `yt-dlp` in your system `PATH` or bundled locally in `./mpv`, `./bin`, or `./tools`.
* **💾 Offline Index Caching:** Builds a local index of anime titles from the site sitemap/crawls, cached for 12 hours so searches feel instant.

---

## 📋 Prerequisites

Before using `waku`, make sure you have the following installed:

1. **[mpv](https://mpv.io/)** — Primary video player.
2. **[yt-dlp](https://github.com/yt-dlp/yt-dlp)** *(Optional, but recommended)* — Handles fallback video hosts like OK.ru, Dailymotion, or Google Drive.

> 💡 **Tip for Windows Users:** You can drop `mpv.com` / `mpv.exe` and `yt-dlp.exe` directly inside a folder named `mpv` or `tools` next to the `waku` binary, and it will pick them up automatically without messing with system environment variables.

---

## 🚀 Installation

### Building from Source

Make sure you have [Rust](https://www.rust-lang.org/tools/install) installed:

```bash
git clone [https://github.com/your-username/waku.git](https://github.com/sapph1rer/Waku-cli.git)
cd waku
cargo install --path .
```

Or build a release binary manually:

```bash
cargo build --release
# The compiled binary will be available at ./target/release/waku
```

---

## 💡 Usage Guide

### Interactive Mode

Simply type `waku` and hit Enter to bring up the search prompt:

```bash
waku
```

Or search directly with a query:

```bash
waku "frieren"
```

### Command Options & Examples

```bash
# Search and launch interactively
waku "naruto"

# Open an anime page directly via URL
waku -u "https://anime-waku.com/anime/frieren-th/"

# Jump directly to episode 5 of a title
waku "frieren" -e 5

# Set preferred video quality (1080, 720, 480, 360, best, worst)
waku "one piece" -q 1080

# Show available server options instead of auto-picking the best working one
waku "bleach" --choose-server

# Print the resolved stream/embed URL to stdout (useful for piping into VLC or downloaders)
waku "frieren" -e 1 --print

# List all matching search results with scores and episode counts without playing
waku "chainsaw man" --list

# Rebuild the local offline search index
waku --reindex
```

---

## ⚙️ Command-Line Flags Reference

| Flag | Long Flag | Description |
| :--- | :--- | :--- |
| `-u` | `--url <URL>` | Open an anime page directly (skips search). |
| `-e` | `--episode <NUM>` | Play a specific episode number directly. |
| `-q` | `--quality <QUAL>` | Video quality (`1080`, `720`, `480`, `360`, `best`, `worst`). |
| `-p` | `--player <NUM>` | Player/server index to use (1-based). |
| | `--choose-server` | Show server selection menu instead of auto-resolving. |
| | `--print` | Output the raw stream URL and exit (doesn't open player). |
| | `--list` | Print ranked search results with match scores. |
| | `--reindex` | Force a rebuild of the local cached anime index. |
| | `--player-cmd <CMD>`| Specify a custom player command (Default: `mpv`). |
| | `--debug-ep <URL>` | Debug an episode URL (saves HTML and tests all video endpoints). |

---

## 🛠️ Environment Variables

* **`WAKU_BASE`**: Override the default site mirror/domain (Default: `https://anime-waku.com`).
  ```bash
  export WAKU_BASE="https://your-mirror-site.com"
  ```

---

## ⚠️ Disclaimer

This tool is created for educational and personal use only. It does not host, stream, or store any video files on its own. All media content is provided by third-party services.

---

## 📄 License

[MIT](LICENSE)
